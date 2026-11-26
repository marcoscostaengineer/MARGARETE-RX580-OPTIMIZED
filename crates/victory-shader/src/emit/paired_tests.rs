//! Console wave32 programs on a host of 64-lane waves alone (GCN): two console waves to a host
//! wave, each half on its own (`Lower::paired`). A group of 128 threads is four console waves in
//! two host waves; what each thread gets is checked against the console's answer for its own
//! console wave, the upper halves above all.

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const THREADS: u32 = 128;
/// What a thread outside EXEC keeps (an inline constant).
const OLD: u32 = 42;

const fn smov(dst: u32, src: u32) -> u32 {
    0xBE80_0300 | dst << 16 | src
}

const fn vmov(dst: u32, src: u32) -> u32 {
    0x7E00_0200 | dst << 17 | src
}

/// The program: `v6 = OLD`; `body`; `buffer_store_dword v6` at the thread's dword. The root
/// constants are s0..s3, the output table s4.
fn native(body: &[u32]) -> NativeProgram {
    let mut words = vec![
        smov(5, 128),
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25,   // s_load_dwordx4 s[8:11], s[4:5], 0
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0 (output byte offset)
        vmov(6, 128 + OLD),
    ];
    words.extend_from_slice(body);
    words.extend([
        0x38 << 26 | 0x1C << 18 | 1 << 12,
        0x80 << 24 | 2 << 16 | 6 << 8 | 7, // buffer_store_dword v6, v7, s[8:11], 0 offen
        0xBF81_0000,
    ]);
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1,
            threads: [THREADS, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `native` lowered for a host of 64-lane waves alone, validated.
fn lowered(native: &NativeProgram) -> HostShader {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resources");
    let io = io::raise(&p, native).expect("io");
    let was = lower::TEST_HOST_WAVE.replace(Some(64));
    let was_min = lower::THREAD_MIN_WAVE.replace(Some(64));
    let result = lower::lower(&lower::Parts {
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    });
    lower::TEST_HOST_WAVE.set(was);
    lower::THREAD_MIN_WAVE.set(was_min);
    let result = result.unwrap_or_else(|e| panic!("lower: {e}"));
    assert_eq!(result.wave_size, Some(64), "two console waves to a host wave of 64");
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: result.dxil,
        user_data: result.user_data,
        samplers: result.samplers,
        static_samplers: Vec::new(),
        root_cbvs: result.root_cbvs,
        root_srvs: result.root_srvs,
        root_uavs: result.root_uavs,
        root_indexed_views: res.root_indexed_views,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone()).unwrap_or_else(|e| panic!("DXIL validation: {e}"));
    }
    shader
}

/// Runs `shader` on the hardware GPU if it runs waves of 64 alone, for each set of root
/// constants: what every thread stored. `None` without such a GPU.
fn run(shader: &HostShader, cases: &[[u32; 4]]) -> Option<Vec<Vec<u32>>> {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    if super::valu_tests::hardware_min_wave() != 64 {
        eprintln!("the hardware device has 32-lane waves (or none): GPU part skipped");
        return None;
    }
    let host = Host::new(false).ok()?;
    let (root, pso) = host.compute_pipeline(shader).expect("the paired pipeline");
    let mut all = Vec::new();
    for constants in cases {
        let mut ud = vec![0; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            ud[u.index as usize] = if u.root == RootType::Constants { constants[u.dword as usize] } else { 0 };
        }
        let out = host
            .dispatch(&root, &pso, &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(THREADS)],
                samplers: &[],
                groups: [1, 1, 1],
            })
            .expect("dispatch");
        all.push(out[0][..THREADS as usize].to_vec());
    }
    Some(all)
}

/// `v_readfirstlane_b32` under EXEC = s2: each console wave's first lane in EXEC, of its own
/// wave (the upper half of a host wave must not read the lower half's first lane).
#[test]
fn a_first_lane_read_is_of_each_console_wave() {
    let shader = lowered(&native(&[
        smov(126, 2),     // exec_lo = s2
        0x7E18_0500,      // v_readfirstlane_b32 s12, v0
        vmov(6, 12),      // v_mov_b32 v6, s12
        smov(126, 193),   // exec_lo = -1
    ]));
    let masks = [u32::MAX, 0x8000_0000, 0x0001_0000, 0xF0F0_0000, 0x0000_0006];
    let cases: Vec<[u32; 4]> = masks.iter().map(|&m| [0, 0, m, 0]).collect();
    let Some(out) = run(&shader, &cases) else { return };
    for (mask, got) in masks.iter().zip(&out) {
        for t in 0..THREADS {
            let lane = t % 32;
            let want = if mask >> lane & 1 == 0 { OLD } else { (t & !31) + mask.trailing_zeros() };
            assert_eq!(got[t as usize], want, "mask {mask:#x}, thread {t}");
        }
    }
    eprintln!("first-lane reads exact in each of four console waves (two host waves of 64)");
}

/// `v_cmp_gt_u32 vcc, s3, v0` then `s_bcnt1_i32_b32` of VCC: the lanes of each console wave
/// whose thread is below s3, counted in that wave alone.
#[test]
fn a_lane_mask_counts_each_console_waves_lanes() {
    let shader = lowered(&native(&[
        0x7D88_0003, // v_cmp_gt_u32 vcc_lo, s3, v0
        0xBE8D_0F6A, // s_bcnt1_i32_b32 s13, vcc_lo
        vmov(6, 13), // v_mov_b32 v6, s13
    ]));
    let limits = [0u32, 1, 31, 32, 33, 50, 64, 70, 96, 127, 128, 200];
    let cases: Vec<[u32; 4]> = limits.iter().map(|&n| [0, 0, 0, n]).collect();
    let Some(out) = run(&shader, &cases) else { return };
    for (n, got) in limits.iter().zip(&out) {
        for t in 0..THREADS {
            let first = t & !31;
            let want = n.saturating_sub(first).min(32);
            assert_eq!(got[t as usize], want, "s3 = {n}, thread {t}");
        }
    }
    eprintln!("lane masks counted in each of four console waves (two host waves of 64)");
}
