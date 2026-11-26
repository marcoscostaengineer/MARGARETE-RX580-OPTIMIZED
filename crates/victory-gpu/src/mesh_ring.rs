//! Where mesh programs in their substitute form (`victory_shader::emit::mesh_sub`) put their
//! records: one device-local buffer, taken in turn by the dispatches of every list.
//!
//! A dispatch takes the next `groups * group_bytes` bytes; at the end the next one starts again
//! from the beginning, with a UAV barrier first, so that what earlier draws of the same list still
//! read is not written over under them. Lists run one after another on the queue; the buffer is
//! large enough (`VICTORY_MESH_RING_MB`, 256 by default) that a list does not come round to the
//! records of one still running.
//!
//! shortcut: a dispatch whose records do not fit an eighth of the buffer runs the groups that do
//! (the rest of its meshlets are not drawn), logged once.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;

pub(crate) struct Ring {
    pub(crate) buffer: ID3D12Resource,
    base: u64,
    size: u64,
    /// Bytes taken so far (wraps by `size`).
    next: AtomicU64,
}

// SAFETY: D3D12 resources are free-threaded; the rest is atomic or fixed.
unsafe impl Send for Ring {}
// SAFETY: as above.
unsafe impl Sync for Ring {}

/// Records start on 256 bytes (a raw view needs 16; a whole cache line is kinder).
const ALIGN: u64 = 256;

fn size() -> u64 {
    static SIZE: OnceLock<u64> = OnceLock::new();
    *SIZE.get_or_init(|| {
        let mb = std::env::var("VICTORY_MESH_RING_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&mb| (16..=4096).contains(&mb))
            .unwrap_or(256);
        mb << 20
    })
}

/// The ring, made at first use; `None` if the host could not make it.
pub(crate) fn ring() -> Option<&'static Ring> {
    static RING: OnceLock<Option<Ring>> = OnceLock::new();
    RING.get_or_init(|| {
        let host = crate::host::host()?;
        let size = size();
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
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: a plain buffer description; the result is checked.
        let made = unsafe {
            host.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                // (A buffer is promoted from the common state at its first use in each list.)
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut buffer,
            )
        };
        if let Err(e) = made {
            tracing::warn!(error = %e, mb = size >> 20, "no buffer for mesh substitute records: their draws are dropped");
            return None;
        }
        let buffer = buffer?;
        crate::host::name(&buffer, || "mesh substitute records".into());
        // SAFETY: a live buffer.
        let base = unsafe { buffer.GetGPUVirtualAddress() };
        tracing::info!(mb = size >> 20, "buffer for mesh substitute records");
        Some(Ring {
            buffer,
            base,
            size,
            next: AtomicU64::new(0),
        })
    })
    .as_ref()
}

impl Ring {
    /// The most bytes one dispatch takes.
    pub(crate) fn most(&self) -> u64 {
        self.size / 8
    }

    /// `bytes` (at most [`Ring::most`]) of records: their GPU address, and whether the ring came
    /// round to its start for them (a UAV barrier must come before their use then).
    pub(crate) fn take(&self, bytes: u64) -> (u64, bool) {
        let bytes = bytes.div_ceil(ALIGN) * ALIGN;
        debug_assert!(bytes <= self.most());
        let mut wrapped = false;
        let start = self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |at| {
                let off = at % self.size;
                Some(if off + bytes > self.size { at + (self.size - off) + bytes } else { at + bytes })
            })
            .unwrap_or_default();
        let mut off = start % self.size;
        if off + bytes > self.size {
            off = 0;
            wrapped = true;
        }
        wrapped |= off == 0 && start != 0;
        (self.base + off, wrapped)
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
