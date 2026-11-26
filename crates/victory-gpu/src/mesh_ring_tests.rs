//! The substitute form's sequence on the card, as `crate::draw` runs it: a compute program writes
//! the groups' records into the ring and raises its draw's instances, then a vertex program draws
//! them, direct and indirect. Small HLSL programs stand in for the translated ones (the records
//! and the argument slot are used as the translator's forms use them). With a diagnostics build
//! and `VICTORY_D3D12_DEBUG=1` the D3D12 debug layer and GPU-based validation watch every barrier
//! and state, and the test fails on any error they report.

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{PCSTR, s};

use super::{ring, ARGS_BYTES};

const SOURCE: &str = r#"
#define RS "RootFlags(ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT), UAV(u0), UAV(u1)"
RWByteAddressBuffer records : register(u0);
RWByteAddressBuffer args : register(u1);
// A group's record: three vertices (positions), 64 bytes.
[RootSignature(RS)]
[numthreads(3, 1, 1)]
void cs(uint3 g : SV_GroupID, uint t : SV_GroupIndex) {
    if (g.x >= args.Load(16)) return;
    if (t == 0) { uint was; args.InterlockedMax(4, g.x + 1, was); }
    float2 p = float2(t == 1 ? 3.0 : -1.0, t == 2 ? 3.0 : -1.0);
    records.Store4(g.x * 64 + t * 16, asuint(float4(p, 0.5, 1.0)));
}
[RootSignature(RS)]
float4 vs(uint v : SV_VertexID, uint i : SV_InstanceID) : SV_Position {
    return asfloat(records.Load4(i * 64 + (v % 3) * 16));
}
[RootSignature(RS)]
float4 ps() : SV_Target { return float4(1.0, 0.5, 0.25, 1.0); }
"#;
const GROUP_BYTES: u64 = 64;
const SIZE: u32 = 16;

fn compile(entry: PCSTR, target: PCSTR) -> Vec<u8> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let made = unsafe {
        D3DCompile(SOURCE.as_ptr().cast(), SOURCE.len(), s!("substitute"), None, None, entry, target, 0, 0, &mut code, Some(&mut errors))
    };
    if made.is_err() {
        // SAFETY: the blob's own bytes.
        let text = errors.map(|e| unsafe {
            String::from_utf8_lossy(std::slice::from_raw_parts(e.GetBufferPointer().cast::<u8>(), e.GetBufferSize())).into_owned()
        });
        panic!("HLSL: {text:?}");
    }
    let code = code.expect("code");
    // SAFETY: the blob's own bytes.
    unsafe { std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize()).to_vec() }
}

fn bytecode(code: &[u8]) -> D3D12_SHADER_BYTECODE {
    D3D12_SHADER_BYTECODE {
        pShaderBytecode: code.as_ptr().cast(),
        BytecodeLength: code.len(),
    }
}

fn committed(device: &ID3D12Device, heap: D3D12_HEAP_TYPE, desc: &D3D12_RESOURCE_DESC, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: plain creation.
    unsafe {
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() },
                D3D12_HEAP_FLAG_NONE,
                desc,
                state,
                None,
                &mut made,
            )
            .expect("a resource");
    }
    made.expect("a resource")
}

fn buffer_desc(size: u64) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    }
}

/// Direct: 2 groups dispatched and drawn. Indirect: a record asking for 4 groups with room for
/// 2 (the ones past them return at once), drawn with the instances the dispatch left. Both cover
/// the target with the pixel program's colour; with the debug layer on, it reports no error.
#[test]
fn the_substitute_sequence_draws_with_valid_barriers() {
    let (Some(host), Some(ring)) = (crate::host::host(), ring()) else {
        return; // no host GPU here
    };
    let device = &host.device;
    let cs = compile(s!("cs"), s!("cs_5_1"));
    let vs = compile(s!("vs"), s!("vs_5_1"));
    let ps = compile(s!("ps"), s!("ps_5_1"));
    // SAFETY: plain creation from compiled code holding its root signature.
    let (root, compute, graphics) = unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, &cs).expect("root signature");
        let compute: ID3D12PipelineState = device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: bytecode(&cs),
                ..Default::default()
            })
            .expect("compute pipeline");
        let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            VS: bytecode(&vs),
            PS: bytecode(&ps),
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        desc.RTVFormats[0] = DXGI_FORMAT_R32G32B32A32_FLOAT;
        desc.BlendState.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
        let graphics: ID3D12PipelineState = device.CreateGraphicsPipelineState(&desc).expect("graphics pipeline");
        (root, compute, graphics)
    };
    let _ = host.debug_error();
    for indirect in [false, true] {
        let list = host.new_list().expect("a host list");
        let l = &list.list;
        let target = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: u64::from(SIZE),
                Height: SIZE,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                ..Default::default()
            },
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        // SAFETY: plain creation.
        let rtvs: ID3D12DescriptorHeap = unsafe {
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: 1,
                ..Default::default()
            })
        }
        .expect("RTV heap");
        // The title's indirect record: 4 groups.
        let record = committed(device, D3D12_HEAP_TYPE_UPLOAD, &buffer_desc(256), D3D12_RESOURCE_STATE_GENERIC_READ);
        // SAFETY: a mapped upload buffer of 256 bytes.
        unsafe {
            let mut p = std::ptr::null_mut();
            record.Map(0, None, Some(&mut p)).expect("map");
            std::ptr::copy_nonoverlapping([4u32, 1, 1].as_ptr(), p.cast::<u32>(), 3);
            record.Unmap(0, None);
        }
        let most = 2u32;
        let taken = ring.take(&list, u64::from(most) * GROUP_BYTES);
        let args = if indirect {
            assert!(ring.begin(&list, &taken, 3, most));
            taken.args
        } else {
            ring.direct(&list).expect("the direct slot")
        };
        // SAFETY: an open list; live objects kept to the end of the test.
        unsafe {
            let rtv = rtvs.GetCPUDescriptorHandleForHeapStart();
            device.CreateRenderTargetView(&target, None, rtv);
            l.ClearRenderTargetView(rtv, &[0.0; 4], None);
            l.SetComputeRootSignature(&root);
            l.SetComputeRootUnorderedAccessView(0, taken.records);
            l.SetComputeRootUnorderedAccessView(1, args);
            l.SetPipelineState(&compute);
            if indirect {
                l.ExecuteIndirect(&ring.dispatch, 1, &record, 0, None, 0);
            } else {
                l.Dispatch(most, 1, 1);
            }
        }
        if indirect {
            ring.computed(&list);
        } else {
            ring.computed_records(&list);
        }
        // SAFETY: as above.
        unsafe {
            l.SetGraphicsRootSignature(&root);
            l.SetGraphicsRootUnorderedAccessView(0, taken.records);
            l.SetGraphicsRootUnorderedAccessView(1, args);
            l.SetPipelineState(&graphics);
            let rtv = rtvs.GetCPUDescriptorHandleForHeapStart();
            l.OMSetRenderTargets(1, Some(&rtv), false, None);
            l.RSSetViewports(&[D3D12_VIEWPORT { Width: SIZE as f32, Height: SIZE as f32, MaxDepth: 1.0, ..Default::default() }]);
            l.RSSetScissorRects(&[windows::Win32::Foundation::RECT { right: SIZE as i32, bottom: SIZE as i32, ..Default::default() }]);
            l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            if indirect {
                l.ExecuteIndirect(&ring.draw, 1, &ring.args, taken.args_offset, None, 0);
            } else {
                l.DrawInstanced(3, most, 0, 0);
            }
        }
        if indirect {
            ring.drawn(&list);
        }
        let pitch = u64::from(SIZE) * 16;
        let back = host.read_back_buffer(pitch.next_multiple_of(256) * u64::from(SIZE)).expect("a read back");
        // SAFETY: as above.
        unsafe {
            l.ResourceBarrier(&[crate::host::transition(&target, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&back),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                            Width: SIZE,
                            Height: SIZE,
                            Depth: 1,
                            RowPitch: pitch.next_multiple_of(256) as u32,
                        },
                    },
                },
            };
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&target),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            l.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        }
        list.keep(&target);
        list.keep(&record);
        list.keep(&compute);
        list.keep(&graphics);
        list.keep(&root);
        list.close();
        host.execute(&[&list]);
        // SAFETY: a plain query.
        assert!(unsafe { device.GetDeviceRemovedReason() }.is_ok(), "the card is alive");
        if let Some(error) = host.debug_error() {
            panic!("indirect {indirect}: the debug layer reports: {error}");
        }
        // SAFETY: the read back the card has written and is done with.
        let texels: Vec<[f32; 4]> = unsafe {
            let mut p = std::ptr::null_mut();
            back.Map(0, None, Some(&mut p)).expect("map");
            let rows = (0..SIZE as usize)
                .flat_map(|y| {
                    std::slice::from_raw_parts(p.cast::<u8>().add(y * pitch.next_multiple_of(256) as usize).cast::<[f32; 4]>(), SIZE as usize)
                        .to_vec()
                })
                .collect();
            back.Unmap(0, None);
            rows
        };
        let lit = texels.iter().filter(|t| **t == [1.0, 0.5, 0.25, 1.0]).count();
        assert_eq!(lit, (SIZE * SIZE) as usize, "indirect {indirect}: texels drawn");
        assert_eq!(ARGS_BYTES, 32);
    }
    eprintln!(
        "substitute sequence drew, direct and indirect; debug layer {}",
        if host.debug_layer() { "on, no errors" } else { "off" }
    );
}
