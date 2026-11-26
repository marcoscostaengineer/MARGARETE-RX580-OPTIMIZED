//! Mesh shaders on a host without them (`MeshShaderTier` 0: GCN cards such as
//! the RX 580, Pascal and older): each translated mesh program is emitted a
//! second time as a compute program, and a small vertex program reads what it
//! wrote.
//!
//! - **The compute program** is the mesh program's own lowering with its
//!   outputs sent to the root UAV `u<SLOT_MESH_OUT>` (a raw buffer) instead of
//!   the mesh outputs: `setMeshOutputCounts` is left out, `emitIndices`,
//!   `storeVertexOutput` and `storePrimitiveOutput` become raw stores into the
//!   group's record ([`MeshLayout`]). Every primitive starts culled, as in the
//!   mesh form; the program's own primitive exports uncull the ones it makes.
//! - **The vertex program** ([`vertex_program`]) is drawn as
//!   `DrawInstanced(max_primitives * 3, groups)`: instance `g` is group `g`,
//!   vertex `3p + c` corner `c` of its primitive `p`. It reads the corner's
//!   vertex out of the record and exports it with the per-primitive outputs
//!   beside it (constant, as the pixel program reads them), and puts a culled
//!   primitive's corners at NaN, which the rasteriser drops.
//!
//! The host runs `Dispatch(groups)`, a UAV barrier, then the draw, with the
//! same root arguments for both programs. What it costs: the record is
//! written to memory and read back (the mesh form keeps it on chip), and
//! every primitive slot of every group is drawn, culled ones as three
//! vertices dropped at clipping.

use super::bitcode::{Attr, Bin, Cast, V, pred};
use super::container::SigElement;
use super::shader::Shader;

/// The root UAV the compute form writes its records to and the vertex form
/// reads them from (`u<SLOT_MESH_OUT>` in `ROOT_CBV_SPACE`).
pub const SLOT_MESH_OUT: u8 = 252;

/// The root UAV of the dispatch's argument slot ([`ARGS_BYTES`] bytes): dwords 0 to 3 the
/// arguments of the draw that follows (vertices an instance, instances, 0, 0), dword 4 the most
/// groups its records hold. The host writes the slot before the dispatch with no instances; the
/// first thread of every group that runs raises the instances to its group + 1, so an indirect
/// dispatch leaves the draw of its groups behind it. Groups past the most return at once.
pub const SLOT_MESH_ARGS: u8 = 251;
/// Byte of the instance count in the argument slot.
pub const ARGS_INSTANCES: u32 = 4;
/// Byte of the most groups in the argument slot.
pub const ARGS_MOST: u32 = 16;
/// Bytes of an argument slot.
pub const ARGS_BYTES: u32 = 32;

thread_local! {
    static AS_COMPUTE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The layout of the last mesh program lowered in its compute form here.
    static LAST: std::cell::RefCell<Option<MeshLayout>> = const { std::cell::RefCell::new(None) };
}

/// Notes the layout of the mesh program just lowered in its compute form.
pub(crate) fn lowered(layout: MeshLayout) {
    LAST.set(Some(layout));
}

/// The layout [`lowered`] noted last on this thread, taken.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn take_last() -> Option<MeshLayout> {
    LAST.take()
}

/// A mesh program's compute form and the layout of the records it writes.
#[derive(Debug, Clone)]
pub struct MeshSubstitute {
    pub compute: super::HostShader,
    pub layout: MeshLayout,
}

/// Translates the mesh program of console container `container` in its
/// compute form (with the same second tries as [`super::translate`]), with
/// the layout [`super::mesh_layout`] reads from the container, which is the
/// one the translation used.
pub fn translate(container: &[u8]) -> Result<MeshSubstitute, crate::ShaderError> {
    LAST.set(None);
    let compute = as_compute(|| super::translate(container))?;
    let layout = super::mesh_layout(container)?;
    let lowered = LAST.take().ok_or(crate::ShaderError::Unsupported {
        offset: 0,
        what: "a mesh substitute of a program that is no mesh program",
    })?;
    if !same_places(&layout, &lowered) {
        return Err(crate::ShaderError::Unsupported {
            offset: 0,
            what: "a mesh substitute whose record the container does not describe",
        });
    }
    Ok(MeshSubstitute { compute, layout })
}

/// Whether two layouts put every output in the same place (usage aside).
pub(crate) fn same_places(a: &MeshLayout, b: &MeshLayout) -> bool {
    let names = |els: &[SigElement]| els.iter().map(|e| (e.name.clone(), e.index, e.kind)).collect::<Vec<_>>();
    (a.max_vertices, a.max_primitives) == (b.max_vertices, b.max_primitives)
        && names(&a.vertex) == names(&b.vertex)
        && names(&a.prims) == names(&b.prims)
}

/// Runs `f` with mesh programs lowered as their compute form.
pub(crate) fn as_compute<T>(f: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            AS_COMPUTE.set(self.0);
        }
    }
    let _reset = Reset(AS_COMPUTE.replace(true));
    f()
}

static HOST_MESH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Tells the translator whether the host runs mesh shaders (the default):
/// without them, mesh programs translate to their compute form.
pub fn set_host_mesh_shaders(on: bool) {
    HOST_MESH.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the host runs mesh shaders, as [`set_host_mesh_shaders`] said.
pub fn host_mesh_shaders() -> bool {
    HOST_MESH.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether mesh programs lowered now take their compute form.
pub(crate) fn on() -> bool {
    AS_COMPUTE.get() || !host_mesh_shaders()
}

/// Where a group's outputs lie in its record, in dwords from the record's
/// start: `max_vertices` vertices of [`MeshLayout::vertex_dwords`] each, then
/// `max_primitives` primitives of [`MeshLayout::prim_dwords`] each. An output
/// element `n` takes the four dwords `4n..4n + 4` of its vertex or primitive
/// (its own columns among them); a primitive's first four dwords are its
/// three corner indices and a culled flag (non-zero: culled), its elements
/// follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshLayout {
    pub max_vertices: u32,
    pub max_primitives: u32,
    /// Per-vertex output elements, as the compute form numbers them.
    pub(crate) vertex: Vec<SigElement>,
    /// Per-primitive output elements, likewise.
    pub(crate) prims: Vec<SigElement>,
}

impl MeshLayout {
    pub fn vertex_dwords(&self) -> u32 {
        4 * self.vertex.len() as u32
    }

    pub fn prim_dwords(&self) -> u32 {
        4 + 4 * self.prims.len() as u32
    }

    /// Bytes of one group's record.
    pub fn group_bytes(&self) -> u32 {
        4 * (self.max_vertices * self.vertex_dwords() + self.max_primitives * self.prim_dwords())
    }

    /// Vertices the draw of one group asks for (an instance per group).
    pub fn vertices_per_group(&self) -> u32 {
        3 * self.max_primitives
    }

    /// Byte of dword `dword` of vertex `vertex` in the group whose record
    /// starts at `base`.
    pub(crate) fn vertex_byte(&self, s: &mut Shader, block: usize, base: V, vertex: V, dword: u32) -> V {
        let stride = s.m.i32(4 * self.vertex_dwords());
        let at = s.m.bin(block, Bin::Mul, vertex, stride);
        let at = s.m.bin(block, Bin::Add, base, at);
        let k = s.m.i32(4 * dword);
        s.m.bin(block, Bin::Add, at, k)
    }

    /// Byte of dword `dword` of primitive `prim`, likewise.
    pub(crate) fn prim_byte(&self, s: &mut Shader, block: usize, base: V, prim: V, dword: u32) -> V {
        let stride = s.m.i32(4 * self.prim_dwords());
        let at = s.m.bin(block, Bin::Mul, prim, stride);
        let first = s.m.i32(4 * (self.max_vertices * self.vertex_dwords() + dword));
        let at = s.m.bin(block, Bin::Add, at, first);
        s.m.bin(block, Bin::Add, base, at)
    }
}

/// The byte where group `group`'s record starts.
pub(crate) fn group_base(s: &mut Shader, block: usize, layout: &MeshLayout, group: V) -> V {
    let bytes = s.m.i32(layout.group_bytes());
    s.m.bin(block, Bin::Mul, group, bytes)
}

/// Stores `values` (i32, at most four) at `byte` of the record buffer.
pub(crate) fn store(s: &mut Shader, block: usize, byte: V, values: &[V]) {
    debug_assert!((1..=4).contains(&values.len()));
    let h = s.root_uav_handle(SLOT_MESH_OUT);
    let undef = s.m.undef(s.t.i32);
    let mut args = vec![h, byte, undef];
    for k in 0..4 {
        args.push(values.get(k).copied().unwrap_or(undef));
    }
    let mask = s.m.i8((1u8 << values.len()) - 1);
    let align = s.m.i32(4);
    args.extend([mask, align]);
    let void = s.t.void;
    s.op(block, "rawBufferStore.i32", Attr::NoUnwind, void, 140, &args);
}

/// Loads `n` dwords (at most four) at `byte` of the record buffer, as i32.
fn load(s: &mut Shader, block: usize, byte: V, n: u32) -> Vec<V> {
    load_from(s, block, SLOT_MESH_OUT, byte, n)
}

/// The most groups the dispatch's records hold ([`ARGS_MOST`]).
pub(crate) fn most_groups(s: &mut Shader, block: usize) -> V {
    let byte = s.m.i32(ARGS_MOST);
    load_from(s, block, SLOT_MESH_ARGS, byte, 1)[0]
}

/// Raises the draw's instances to `instances` ([`ARGS_INSTANCES`]).
pub(crate) fn raise_instances(s: &mut Shader, block: usize, instances: V) {
    let h = s.root_uav_handle(SLOT_MESH_ARGS);
    let i32t = s.t.i32;
    let undef = s.m.undef(i32t);
    let (umax, byte) = (s.m.i32(7), s.m.i32(ARGS_INSTANCES));
    s.op(block, "atomicBinOp.i32", Attr::NoUnwind, i32t, 78, &[h, umax, byte, undef, undef, instances]);
}

/// Loads `n` dwords (at most four) at `byte` of the raw root UAV `slot`, as i32.
fn load_from(s: &mut Shader, block: usize, slot: u8, byte: V, n: u32) -> Vec<V> {
    let h = s.root_uav_handle(slot);
    let undef = s.m.undef(s.t.i32);
    let mask = s.m.i8(((1u32 << n) - 1) as u8);
    let align = s.m.i32(4);
    let res_ret = s.t.res_ret;
    let r = s.op(block, "rawBufferLoad.f32", Attr::ReadOnly, res_ret, 139, &[h, byte, undef, mask, align]);
    (0..n)
        .map(|k| {
            let f = s.m.extract(block, r, k);
            let i32t = s.t.i32;
            s.m.cast(block, Cast::Bitcast, f, i32t)
        })
        .collect()
}

/// Rows the vertex form exports the per-primitive elements at: their own
/// where those follow the vertex elements' (as DXC packs a mesh program's
/// signatures), else after the last vertex row.
fn prim_rows(layout: &MeshLayout) -> Vec<Option<u32>> {
    let end = |e: &SigElement| e.row.map(|r| r + 1).unwrap_or(0);
    let after = layout.vertex.iter().map(end).max().unwrap_or(0);
    let own_ok = layout.prims.iter().all(|p| p.row.is_none_or(|r| r >= after));
    let first = layout.prims.iter().filter_map(|p| p.row).min().unwrap_or(0);
    layout
        .prims
        .iter()
        .map(|p| p.row.map(|r| if own_ok { r } else { r - first + after }))
        .collect()
}

/// The vertex program that draws the records of the compute form of a mesh
/// program with layout `layout`: a DXIL container (SM 6.6).
pub fn vertex_program(layout: &MeshLayout) -> Vec<u8> {
    let mut s = Shader::new(false);
    let e = s.entry;
    let i32t = s.t.i32;
    let vid = SigElement {
        name: "SV_VertexID".into(),
        index: 0,
        kind: 1,
        comp: 5,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 1,
        usage: 1,
        half: false,
    };
    let iid = SigElement {
        name: "SV_InstanceID".into(),
        kind: 2,
        row: Some(1),
        ..vid.clone()
    };
    s.inputs = vec![vid, iid];
    let input = |s: &mut Shader, id: u32| {
        let args = [s.m.i32(id), s.m.i32(0), s.m.i8(0), s.m.undef(i32t)];
        s.op(e, "loadInput.i32", Attr::ReadNone, i32t, 4, &args)
    };
    let vertex_id = input(&mut s, 0);
    let group = input(&mut s, 1);
    let three = s.m.i32(3);
    let prim = s.m.bin(e, Bin::UDiv, vertex_id, three);
    let corner = s.m.bin(e, Bin::URem, vertex_id, three);
    let base = group_base(&mut s, e, layout, group);
    let at = layout.prim_byte(&mut s, e, base, prim, 0);
    let head = load(&mut s, e, at, 4);
    let (one, two) = (s.m.i32(1), s.m.i32(2));
    let is1 = s.m.cmp(e, pred::EQ, corner, one);
    let is2 = s.m.cmp(e, pred::EQ, corner, two);
    let index = s.m.select(e, is1, head[1], head[0]);
    let index = s.m.select(e, is2, head[2], index);
    let zero = s.m.i32(0);
    let culled = s.m.cmp(e, pred::NE, head[3], zero);
    let nan = s.m.f32(0x7FC0_0000);

    let mut outputs = layout.vertex.clone();
    for el in &mut outputs {
        el.usage = (1u8 << el.cols) - 1;
    }
    let rows = prim_rows(layout);
    let mut prims = layout.prims.clone();
    for (el, row) in prims.iter_mut().zip(rows) {
        el.row = row;
        el.usage = (1u8 << el.cols) - 1;
        el.interp = 1;
    }
    let store_element =|s: &mut Shader, id: usize, el: &SigElement, byte: V, position: bool| {
        let cols = u32::from(el.col + el.cols);
        let dwords = load(s, e, byte, cols);
        for col in el.col..el.col + el.cols {
            let x = dwords[usize::from(col)];
            let (suffix, x) = if el.comp == 9 {
                let f32t = s.t.f32;
                let f = s.m.cast(e, Cast::Bitcast, x, f32t);
                (".f32", if position { s.m.select(e, culled, nan, f) } else { f })
            } else {
                (".i32", x)
            };
            let args = [s.m.i32(id as u32), s.m.i32(0), s.m.i8(col), x];
            let void = s.t.void;
            s.op(e, &format!("storeOutput{suffix}"), Attr::NoUnwind, void, 5, &args);
        }
    };
    for (n, el) in outputs.iter().enumerate() {
        let byte = layout.vertex_byte(&mut s, e, base, index, 4 * n as u32);
        store_element(&mut s, n, el, byte, el.kind == 3);
    }
    for (n, el) in prims.iter().enumerate() {
        let byte = layout.prim_byte(&mut s, e, base, prim, 4 + 4 * n as u32);
        store_element(&mut s, outputs.len() + n, el, byte, false);
    }
    s.outputs = outputs;
    s.outputs.extend(prims);
    s.m.terminate(e, super::bitcode::Term::Ret);
    s.finish()
}
