//! The Windows client-mode GPU frame tap: while a share is live, each new
//! WGC frame is copied on the GPU into a small pool of shared textures, and
//! the encoder thread opens those textures on its own device. No per-frame
//! BGRA clone and re-upload, unlike the CPU copy it replaces (which stays as
//! the fallback).
//!
//! * The pool lives on the capture device: [`POOL_DEPTH`] BGRA textures,
//!   each shared through an NT handle and guarded by a keyed mutex (key 0 on
//!   both sides; [`SlotStates`] orders who may touch which slot).
//! * The capture thread ([`Producer`]) copies the same resource its staging
//!   readback reads -- the source texture, or a Petal View region's canvas --
//!   into a free slot. No free slot drops the frame: the tap's mailbox only
//!   keeps the latest one anyway.
//! * A [`GpuFrame`] is a lease on one slot; the slot is free again once the
//!   mailbox and the encoder have both let go of it.
//! * The encoder device is created on the capture device's adapter (by
//!   LUID, which the tap learns from the first frames) and opens the pool
//!   with `OpenSharedResource1` (`windows_encode::Converter::upload_texture`).
//!   Until it has, frames take the CPU path.
//! * The pool is rebuilt when the frame size changes and released with the
//!   capture session (docs/WINDOWS_MEDIA_LIFECYCLE.md). If the adapters
//!   differ, or a shared texture can't be created or opened, the stream
//!   falls back to the CPU copy and says so once in the log.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIKeyedMutex, IDXGIResource1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE,
};

use super::tap::LiveTap;

/// Shared textures per pool: one in the mailbox, one being encoded, one
/// being written.
pub const POOL_DEPTH: usize = 3;
/// How long the capture thread waits for a slot's mutex (the slot is free
/// by [`SlotStates`]; this only covers the encoder's copy still in flight).
const PRODUCER_ACQUIRE_MS: u32 = 5;
/// How long the encoder waits for a slot the capture thread released.
pub const CONSUMER_ACQUIRE_MS: u32 = 100;
/// `AcquireSync` timing out and an abandoned mutex are success codes, not
/// errors.
const WAIT_TIMEOUT_HR: i32 = 0x0000_0102;
const WAIT_ABANDONED_HR: i32 = 0x0000_0080;

static NEXT_POOL_ID: AtomicU64 = AtomicU64::new(1);

/// Which pool slots are taken. A slot is acquired by the capture thread
/// before it writes, and released when the last [`GpuFrame`] for it drops.
#[derive(Debug, Default)]
pub struct SlotStates {
    busy: [AtomicBool; POOL_DEPTH],
}

impl SlotStates {
    /// Take the first free slot, if any.
    pub fn acquire(&self) -> Option<usize> {
        self.busy
            .iter()
            .position(|slot| slot.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok())
    }

    pub fn release(&self, index: usize) {
        if let Some(slot) = self.busy.get(index) {
            slot.store(false, Ordering::Release);
        }
    }

    pub fn in_use(&self) -> usize {
        self.busy.iter().filter(|slot| slot.load(Ordering::Acquire)).count()
    }
}

pub fn luid_to_i64(luid: LUID) -> i64 {
    (i64::from(luid.HighPart) << 32) | i64::from(luid.LowPart)
}

/// The adapter a device runs on.
pub fn adapter_luid(device: &ID3D11Device) -> Option<i64> {
    let dxgi: IDXGIDevice = device.cast().ok()?;
    // SAFETY: plain COM queries on a live device.
    let adapter = unsafe { dxgi.GetAdapter() }.ok()?;
    let desc = unsafe { adapter.GetDesc() }.ok()?;
    Some(luid_to_i64(desc.AdapterLuid))
}

/// `IDXGIKeyedMutex::AcquireSync` with its timeout told apart from success:
/// `Ok(true)` acquired, `Ok(false)` timed out.
pub fn acquire_sync(mutex: &IDXGIKeyedMutex, key: u64, milliseconds: u32) -> Result<bool, String> {
    // SAFETY: a direct vtable call on a live interface; the windows crate's
    // wrapper folds WAIT_TIMEOUT (a success HRESULT) into `Ok(())`.
    let hr = unsafe { (Interface::vtable(mutex).AcquireSync)(Interface::as_raw(mutex), key, milliseconds) };
    match hr.0 {
        WAIT_TIMEOUT_HR => Ok(false),
        WAIT_ABANDONED_HR => Err("the shared frame's other device went away".into()),
        _ => hr.ok().map(|()| true).map_err(|error| format!("AcquireSync: {error}")),
    }
}

struct SharedSlot {
    texture: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    /// The NT handle, as an integer (`HANDLE` itself isn't `Send`).
    handle: isize,
}

/// The pool on the capture device. Dropping it closes the NT handles;
/// textures the encoder already opened stay valid on its side.
pub struct SharedPool {
    id: u64,
    size: (u32, u32),
    luid: Option<i64>,
    slots: Vec<SharedSlot>,
    states: SlotStates,
}

impl Drop for SharedPool {
    fn drop(&mut self) {
        for slot in &self.slots {
            // SAFETY: each handle came from CreateSharedHandle and is closed
            // exactly once, here.
            let _ = unsafe { CloseHandle(HANDLE(slot.handle as *mut core::ffi::c_void)) };
        }
    }
}

impl SharedPool {
    pub fn new(device: &ID3D11Device, size: (u32, u32)) -> Result<Self, String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: size.0,
            Height: size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
        };
        let mut slots = Vec::with_capacity(POOL_DEPTH);
        for _ in 0..POOL_DEPTH {
            let mut texture = None;
            // SAFETY: desc is fully initialized; no initial data.
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
                .map_err(|e| format!("CreateTexture2D (shared): {e}"))?;
            let texture: ID3D11Texture2D = texture.ok_or("CreateTexture2D (shared) returned nothing")?;
            let mutex: IDXGIKeyedMutex = texture.cast().map_err(|e| format!("IDXGIKeyedMutex: {e}"))?;
            let resource: IDXGIResource1 = texture.cast().map_err(|e| format!("IDXGIResource1: {e}"))?;
            // SAFETY: no security attributes and no name: a process-local handle.
            let handle = unsafe {
                resource.CreateSharedHandle(None, (DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE).0, None)
            }
            .map_err(|e| format!("CreateSharedHandle: {e}"))?;
            slots.push(SharedSlot { texture, mutex, handle: handle.0 as isize });
        }
        Ok(Self {
            id: NEXT_POOL_ID.fetch_add(1, Ordering::Relaxed),
            size,
            luid: adapter_luid(device),
            slots,
            states: SlotStates::default(),
        })
    }

    /// Copy `size` pixels from the top-left of `source` into slot `index`.
    /// `Ok(false)` when the slot's mutex didn't come free in time.
    fn copy_into(&self, context: &ID3D11DeviceContext, index: usize, source: &ID3D11Resource) -> Result<bool, String> {
        let slot = &self.slots[index];
        if !acquire_sync(&slot.mutex, 0, PRODUCER_ACQUIRE_MS)? {
            return Ok(false);
        }
        let source_box = D3D11_BOX { left: 0, top: 0, front: 0, right: self.size.0, bottom: self.size.1, back: 1 };
        // SAFETY: same format, the box lies inside both textures, and the
        // destination's keyed mutex is held for the copy.
        unsafe {
            context.CopySubresourceRegion(&slot.texture, 0, 0, 0, 0, source, 0, Some(&source_box));
            slot.mutex.ReleaseSync(0).map_err(|e| format!("ReleaseSync: {e}"))?;
        }
        Ok(true)
    }
}

/// A lease on one pool slot holding one frame. The slot is free again when
/// this drops.
pub struct GpuFrame {
    pool: Arc<SharedPool>,
    index: usize,
}

impl Drop for GpuFrame {
    fn drop(&mut self) {
        self.pool.states.release(self.index);
    }
}

impl std::fmt::Debug for GpuFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuFrame").field("pool", &self.pool.id).field("slot", &self.index).finish()
    }
}

impl GpuFrame {
    pub fn pool_id(&self) -> u64 {
        self.pool.id
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn size(&self) -> (u32, u32) {
        self.pool.size
    }

    /// The slot's NT handle, valid while the pool lives (this frame keeps it
    /// alive).
    pub fn shared_handle(&self) -> HANDLE {
        HANDLE(self.pool.slots[self.index].handle as *mut core::ffi::c_void)
    }
}

/// The capture thread's half: one per WGC capture session, dropped with it
/// (and the pool with it, once the last frame lets go).
#[derive(Default)]
pub struct Producer {
    pool: Option<Arc<SharedPool>>,
    /// The capture device's adapter, looked up once.
    luid: Option<Option<i64>>,
    /// The GPU path failed for this capture: the CPU copy carries on.
    disabled: bool,
}

impl Producer {
    /// Offer the frame in `source` (`size`, at its top-left) to the live tap,
    /// if share `token` is the one streaming and the encoder takes GPU
    /// frames. Otherwise the share's CPU copy feeds the tap as before.
    pub fn offer(
        &mut self,
        token: u32,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: &ID3D11Resource,
        size: (u32, u32),
    ) {
        let Some(tap) = super::tap::gpu_target(token) else { return };
        let luid = *self.luid.get_or_insert_with(|| adapter_luid(device));
        if let Some(luid) = luid {
            tap.note_capture_adapter(luid);
        }
        if self.disabled {
            return;
        }
        let Some(encoder) = tap.gpu_encoder_adapter() else { return };
        if luid != Some(encoder) {
            return self.fall_back(&tap, "the capture and encoder devices are on different adapters");
        }
        if self.pool.as_ref().is_none_or(|pool| pool.size != size) {
            // Release the old pool before building the new one.
            self.pool = None;
            match SharedPool::new(device, size) {
                Ok(pool) => self.pool = Some(Arc::new(pool)),
                Err(error) => return self.fall_back(&tap, &error),
            }
        }
        let Some(pool) = self.pool.clone() else { return };
        // Every slot taken: drop this frame, a newer one is coming.
        let Some(index) = pool.states.acquire() else { return };
        let frame = GpuFrame { pool, index };
        match frame.pool.copy_into(context, index, source) {
            Ok(true) => super::tap::offer_gpu_frame(&tap, token, frame),
            Ok(false) => {}
            Err(error) => self.fall_back(&tap, &error),
        }
    }

    fn fall_back(&mut self, tap: &LiveTap, error: &str) {
        self.disabled = true;
        self.pool = None;
        tap.gpu_fell_back(error);
    }
}

/// A hardware D3D11 device, standing in for a capture device.
#[cfg(test)]
pub(crate) fn test_device() -> (ID3D11Device, ID3D11DeviceContext) {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION};
    let mut device = None;
    let mut context = None;
    // SAFETY: valid out-params.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .expect("D3D11CreateDevice");
    (device.expect("device"), context.expect("context"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_handed_out_once_and_come_back() {
        let states = SlotStates::default();
        assert_eq!(states.acquire(), Some(0));
        assert_eq!(states.acquire(), Some(1));
        assert_eq!(states.acquire(), Some(2));
        // All taken (mailbox, encoder, writer): the next frame is dropped.
        assert_eq!(states.acquire(), None);
        assert_eq!(states.in_use(), POOL_DEPTH);
        // The mailbox moved on from slot 1: it is the next one written.
        states.release(1);
        assert_eq!(states.acquire(), Some(1));
        states.release(0);
        states.release(2);
        assert_eq!(states.in_use(), 1);
        // Releasing a free or out-of-range slot changes nothing.
        states.release(0);
        states.release(POOL_DEPTH + 4);
        assert_eq!(states.in_use(), 1);
    }

    /// The lease side, on a real GPU: a frame holds its slot until the last
    /// holder (mailbox or encoder) drops it.
    #[test]
    #[ignore = "needs a D3D11 device"]
    fn a_frame_holds_its_slot_until_the_last_holder_drops() {
        let (device, _) = test_device();
        let pool = Arc::new(SharedPool::new(&device, (64, 32)).unwrap());
        let index = pool.states.acquire().unwrap();
        let frame = Arc::new(GpuFrame { pool: pool.clone(), index });
        let held_by_encoder = frame.clone();
        assert_eq!(pool.states.in_use(), 1);
        drop(frame);
        assert_eq!(pool.states.in_use(), 1, "the encoder still holds it");
        drop(held_by_encoder);
        assert_eq!(pool.states.in_use(), 0);
    }
}
