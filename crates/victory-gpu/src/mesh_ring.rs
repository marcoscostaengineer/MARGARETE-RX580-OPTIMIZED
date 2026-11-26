//! Where mesh programs in their substitute form (`victory_shader::emit::mesh_sub`) put their
//! records and their draw's arguments: two device-local buffers, taken in turn by the dispatches
//! of every list.
//!
//! - **Records** (`VICTORY_MESH_RING_MB`, 512 by default): a dispatch takes the bytes of its
//!   groups' records (an indirect one, whose group count the CPU does not know, the most one
//!   dispatch may take: a sixteenth of the buffer). At the end the next one starts again from the
//!   beginning.
//! - **Argument slots** ([`ARGS_BYTES`] each): the draw's arguments and the most groups the
//!   records hold, written by the list before the dispatch (`WriteBufferImmediate`), raised by the
//!   compute form, read by the draw (`ExecuteIndirect`).
//!
//! States, per list: both buffers decay to the common state when a list has run. At its first
//! use in a list each is moved out of it (with a UAV barrier first, so that a list does not write
//! records an earlier one still reads); the records stay in the unordered-access state, an
//! argument slot goes copy destination, unordered access, indirect argument and back to copy
//! destination around each dispatch.
//!
//! shortcut: an indirect dispatch whose records pass the most a dispatch may take runs the groups
//! that fit (the rest of its meshlets are not drawn); a direct one likewise, logged once.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use victory_shader::emit::mesh_sub::ARGS_BYTES;
use windows::Win32::Graphics::Direct3D12::*;

use crate::host::HostList;

pub(crate) struct Ring {
    pub(crate) records: ID3D12Resource,
    records_base: u64,
    records_size: u64,
    /// Record bytes taken so far (wraps by `records_size`).
    next: AtomicU64,
    pub(crate) args: ID3D12Resource,
    args_base: u64,
    /// Argument slots taken so far (wraps by [`ARGS_SLOTS`]).
    next_args: AtomicU64,
    /// `ExecuteIndirect` of one dispatch record (the title's mesh dispatch records serve as they
    /// are: x, y, z).
    pub(crate) dispatch: ID3D12CommandSignature,
    /// `ExecuteIndirect` of one draw record (an argument slot's first four dwords).
    pub(crate) draw: ID3D12CommandSignature,
    /// The argument slot of direct dispatches ([`Ring::direct`]).
    direct: ID3D12Resource,
    direct_at: u64,
}

// SAFETY: D3D12 objects are free-threaded; the rest is atomic or fixed.
unsafe impl Send for Ring {}
// SAFETY: as above.
unsafe impl Sync for Ring {}

/// Records start on 256 bytes (a raw view needs 16; a whole cache line is kinder).
const ALIGN: u64 = 256;
/// Argument slots in their buffer.
const ARGS_SLOTS: u64 = 1 << 16;
/// [`HostList::first_use`] keys of the two buffers.
const RECORDS_USE: usize = usize::MAX - 0x4d53_0001;
const ARGS_USE: usize = usize::MAX - 0x4d53_0002;
const DIRECT_USE: usize = usize::MAX - 0x4d53_0003;
/// The most groups a mesh dispatch runs on the host.
const MOST_GROUPS: u32 = 65_535;

fn records_size() -> u64 {
    static SIZE: OnceLock<u64> = OnceLock::new();
    *SIZE.get_or_init(|| {
        let mb = std::env::var("VICTORY_MESH_RING_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&mb| (16..=4096).contains(&mb))
            .unwrap_or(512);
        mb << 20
    })
}

fn buffer(device: &ID3D12Device, size: u64, name: &str) -> Option<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: a plain buffer description; the result is checked.
    let ok = unsafe {
        device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut made)
    };
    if let Err(e) = ok {
        tracing::warn!(error = %e, mb = size >> 20, "no buffer for {name}: mesh substitute draws are dropped");
        return None;
    }
    let made = made?;
    crate::host::name(&made, || name.to_owned());
    Some(made)
}

fn signature(device: &ID3D12Device, kind: D3D12_INDIRECT_ARGUMENT_TYPE, stride: u32) -> Option<ID3D12CommandSignature> {
    let arg = D3D12_INDIRECT_ARGUMENT_DESC {
        Type: kind,
        ..Default::default()
    };
    let desc = D3D12_COMMAND_SIGNATURE_DESC {
        ByteStride: stride,
        NumArgumentDescs: 1,
        pArgumentDescs: &arg,
        NodeMask: 0,
    };
    let mut sig: Option<ID3D12CommandSignature> = None;
    // SAFETY: the description points at live data; no root arguments, so no root signature.
    match unsafe { device.CreateCommandSignature(&desc, None, &mut sig) } {
        Ok(()) => sig,
        Err(e) => {
            tracing::warn!(error = %e, "no command signature for mesh substitutes");
            None
        }
    }
}

/// The ring, made at first use; `None` if the host could not make it.
pub(crate) fn ring() -> Option<&'static Ring> {
    static RING: OnceLock<Option<Ring>> = OnceLock::new();
    RING.get_or_init(|| {
        let device = &crate::host::host()?.device;
        let size = records_size();
        let records = buffer(device, size, "mesh substitute records")?;
        let args = buffer(device, ARGS_SLOTS * u64::from(ARGS_BYTES), "mesh substitute draw arguments")?;
        let dispatch = signature(device, D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH, 12)?;
        let draw = signature(device, D3D12_INDIRECT_ARGUMENT_TYPE_DRAW, 16)?;
        let direct = buffer(device, 256, "mesh substitute direct arguments")?;
        // SAFETY: a live buffer.
        let direct_at = unsafe { direct.GetGPUVirtualAddress() };
        tracing::info!(mb = size >> 20, "buffer for mesh substitute records");
        // SAFETY: live buffers.
        let (records_base, args_base) = unsafe { (records.GetGPUVirtualAddress(), args.GetGPUVirtualAddress()) };
        Some(Ring {
            records,
            records_base,
            records_size: size,
            next: AtomicU64::new(0),
            args,
            args_base,
            next_args: AtomicU64::new(0),
            dispatch,
            draw,
            direct,
            direct_at,
        })
    })
    .as_ref()
}

/// What one dispatch was given: where its records and its argument slot lie.
pub(crate) struct Taken {
    pub(crate) records: u64,
    pub(crate) args: u64,
    /// Byte of the argument slot in its buffer (for `ExecuteIndirect`).
    pub(crate) args_offset: u64,
}

impl Ring {
    /// The most record bytes one dispatch takes.
    pub(crate) fn most(&self) -> u64 {
        self.records_size / 16
    }

    /// `bytes` (at most [`Ring::most`]) of records and an argument slot for a dispatch recorded
    /// in `list`, with the barriers their first use in it, or a return to the ring's start,
    /// needs.
    pub(crate) fn take(&self, list: &HostList, bytes: u64) -> Taken {
        let bytes = bytes.div_ceil(ALIGN).max(1) * ALIGN;
        debug_assert!(bytes <= self.most());
        let size = self.records_size;
        let start = self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |at| {
                let off = at % size;
                Some(if off + bytes > size { at + (size - off) + bytes } else { at + bytes })
            })
            .unwrap_or_default();
        let mut off = start % size;
        if off + bytes > size {
            off = 0;
        }
        let wrapped = off == 0 && start != 0;
        let slot = self.next_args.fetch_add(1, Ordering::Relaxed) % ARGS_SLOTS;
        let args_offset = slot * u64::from(ARGS_BYTES);
        let first_records = list.first_use(RECORDS_USE);
        let first_args = list.first_use(ARGS_USE);
        let mut barriers = Vec::new();
        if wrapped || first_records || first_args || slot == 0 {
            // What earlier work wrote or still reads there, done first.
            barriers.push(uav_barrier());
        }
        if first_records {
            barriers.push(crate::host::transition(
                &self.records,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            ));
        }
        if first_args {
            barriers.push(crate::host::transition(
                &self.args,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST,
            ));
        }
        if !barriers.is_empty() {
            // SAFETY: an open list; the buffers live as long as the process.
            unsafe { list.list.ResourceBarrier(&barriers) };
        }
        list.keep(&self.records);
        list.keep(&self.args);
        Taken {
            records: self.records_base + off,
            args: self.args_base + args_offset,
            args_offset,
        }
    }

    /// The argument slot of direct dispatches in `list`: the most groups any dispatch runs, and
    /// instances no draw reads (a direct draw counts its groups on the CPU). Written once a list
    /// and left in the unordered-access state, so a direct dispatch moves no argument slot and
    /// its draw waits for nothing but its records. `None` without `WriteBufferImmediate`.
    pub(crate) fn direct(&self, list: &HostList) -> Option<u64> {
        if list.first_use(DIRECT_USE) {
            list.keep(&self.direct);
            // SAFETY: an open list; the buffer lives as long as the process, in the common
            // state at a list's start.
            unsafe {
                list.list.ResourceBarrier(&[crate::host::transition(
                    &self.direct,
                    D3D12_RESOURCE_STATE_COMMON,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                )]);
            }
            if !write_args(list, self.direct_at, &[0, 0, 0, 0, MOST_GROUPS]) {
                return None;
            }
            // SAFETY: as above.
            unsafe {
                list.list.ResourceBarrier(&[crate::host::transition(
                    &self.direct,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                )]);
            }
        }
        Some(self.direct_at)
    }

    /// After a direct dispatch: its records readable by the draw.
    pub(crate) fn computed_records(&self, list: &HostList) {
        // SAFETY: an open list; the buffer lives as long as the process.
        unsafe { list.list.ResourceBarrier(&[uav_barrier_of(&self.records)]) };
    }

    /// The argument slot `t` before its dispatch: a draw of `vertices` an instance and no
    /// instances yet, of at most `most` groups; then made writable for the dispatch.
    pub(crate) fn begin(&self, list: &HostList, t: &Taken, vertices: u32, most: u32) -> bool {
        if !write_args(list, t.args, &[vertices, 0, 0, 0, most]) {
            return false;
        }
        // SAFETY: an open list; the slot lies in a live buffer.
        unsafe {
            list.list.ResourceBarrier(&[crate::host::transition(
                &self.args,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )]);
        }
        true
    }

    /// After the dispatch: its records readable by the draw, its slot readable as arguments.
    pub(crate) fn computed(&self, list: &HostList) {
        // SAFETY: an open list; the buffers live as long as the process.
        unsafe {
            list.list.ResourceBarrier(&[
                uav_barrier_of(&self.records),
                crate::host::transition(
                    &self.args,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                ),
            ]);
        }
    }

    /// After the draw: the argument slots back to the copy destination state.
    pub(crate) fn drawn(&self, list: &HostList) {
        // SAFETY: as above.
        unsafe {
            list.list.ResourceBarrier(&[crate::host::transition(
                &self.args,
                D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )]);
        }
    }
}

/// Writes `words` from GPU address `at` (in a buffer in the copy destination state) in `list`.
/// False without `WriteBufferImmediate`.
fn write_args(list: &HostList, at: u64, words: &[u32]) -> bool {
    use windows::core::Interface;
    let Ok(list2) = list.list.cast::<ID3D12GraphicsCommandList2>() else {
        static SAID: OnceLock<()> = OnceLock::new();
        SAID.get_or_init(|| tracing::warn!("no WriteBufferImmediate on this host: mesh substitute draws are dropped"));
        return false;
    };
    let params: Vec<D3D12_WRITEBUFFERIMMEDIATE_PARAMETER> = words
        .iter()
        .enumerate()
        .map(|(n, &v)| D3D12_WRITEBUFFERIMMEDIATE_PARAMETER { Dest: at + 4 * n as u64, Value: v })
        .collect();
    // SAFETY: an open list; the caller names a live buffer in the copy destination state.
    unsafe { list2.WriteBufferImmediate(params.len() as u32, params.as_ptr(), None) };
    true
}

/// A UAV barrier over the unordered accesses of `resource` alone.
fn uav_barrier_of(resource: &ID3D12Resource) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                // SAFETY: a borrowed pointer the barrier never releases.
                pResource: unsafe { std::mem::transmute_copy(resource) },
            }),
        },
    }
}

/// A UAV barrier over every unordered access (the ring's records among them).
pub(crate) fn uav_barrier() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: std::mem::ManuallyDrop::new(None),
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The argument slots hold what was written, in two lists one after another (each starts
    /// with both buffers in the common state), through the states a dispatch takes them, and
    /// the card stays alive. A direct slot is written once a list.
    #[test]
    fn argument_slots_hold_what_was_written() {
        let (Some(host), Some(ring)) = (crate::host::host(), ring()) else {
            return; // no host GPU here
        };
        for round in 0..2u32 {
            let list = host.new_list().expect("a host list");
            let direct = ring.direct(&list).expect("the direct slot");
            assert_eq!(ring.direct(&list), Some(direct), "one direct slot a list");
            let t = ring.take(&list, 1000);
            assert!(t.records % ALIGN == 0, "records start on {ALIGN} bytes");
            assert!(ring.begin(&list, &t, 96 + round, 7));
            ring.computed(&list);
            ring.drawn(&list);
            ring.computed_records(&list);
            let back = host.read_back_buffer(64).expect("a read back");
            // SAFETY: an open list; live buffers in the states named.
            unsafe {
                list.list.ResourceBarrier(&[
                    crate::host::transition(&ring.args, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE),
                    crate::host::transition(&ring.direct, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE),
                ]);
                list.list.CopyBufferRegion(&back, 0, &ring.args, t.args_offset, u64::from(ARGS_BYTES));
                list.list.CopyBufferRegion(&back, 32, &ring.direct, 0, 32);
            }
            list.close();
            host.execute(&[&list]);
            // SAFETY: a plain query.
            assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok(), "the card is alive");
            let mut words = [0u32; 16];
            // SAFETY: a read-back buffer of 64 bytes the card has written and is done with.
            unsafe {
                let mut p = std::ptr::null_mut();
                back.Map(0, None, Some(&mut p)).expect("map");
                std::ptr::copy_nonoverlapping(p.cast::<u32>(), words.as_mut_ptr(), 16);
                back.Unmap(0, None);
            }
            assert_eq!(words[..5], [96 + round, 0, 0, 0, 7], "the dispatch's slot");
            assert_eq!(words[8..13], [0, 0, 0, 0, MOST_GROUPS], "the direct slot");
        }
        eprintln!("argument slots checked on the card in two lists");
    }
}
