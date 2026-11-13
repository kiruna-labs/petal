//! The client-mode frame tap: a latest-frame mailbox fed from the bound
//! share's capture callback, before the WebRTC conversion. Binding is by
//! window id (the share token), so a share republished without stopping (a
//! resilience reconnect) feeds the stream again with no extra step. Once the
//! bound share stops, the tap refuses its window until "Resume live stream
//! here" rebinds it: the same window shared again never goes back on air by
//! itself, whether it comes back with a new id (Windows tokens) or the same
//! one (macOS window ids).
//!
//! Only frames that leave Petal's own windows out reach the stream: going live
//! applies the capture exclusion first, and every frame says whether it was
//! taken after that took effect (`excludes_petal`). One that wasn't is dropped
//! here, never queued, so the meeting can't leak onto the stream (#487).
//!
//! The capture callback calls [`offer_frame`] on every frame of every share;
//! when nothing is live, or the share isn't the bound one, that is one
//! relaxed atomic load and the frame is never copied. On Windows the bound
//! share's capture thread copies frames into shared GPU textures instead
//! ([`super::gpu_tap`]) once the encoder takes them; while it does, the CPU
//! copy is skipped.

#[cfg(target_os = "windows")]
use std::sync::atomic::AtomicI64;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::clock::FrameMailbox;
use crate::sync_ext::MutexExt;

/// A tapped capture frame.
#[cfg(not(target_os = "macos"))]
#[derive(Debug)]
pub struct TapFrame {
    pub pixels: TapPixels,
    pub width: u32,
    pub height: u32,
}

#[cfg(not(target_os = "macos"))]
#[derive(Debug)]
pub enum TapPixels {
    /// A CPU copy (BGRA, `bytes_per_row` stride): the fallback.
    Bgra { bgra: Vec<u8>, bytes_per_row: u32 },
    /// A slot of the capture's shared texture pool.
    #[cfg(target_os = "windows")]
    Gpu(super::gpu_tap::GpuFrame),
}

/// The GPU tap's handshake between the capture thread and the encoder.
#[cfg(target_os = "windows")]
#[derive(Debug, Default)]
struct GpuTapState {
    /// The bound share's capture adapter (LUID; 0 = not known yet).
    capture_luid: AtomicI64,
    /// The encoder's adapter once it takes GPU frames (0 = it doesn't).
    encoder_luid: AtomicI64,
    /// The encoder couldn't use a shared frame: CPU copies from now on.
    failed: AtomicBool,
    /// The share whose capture is feeding GPU frames (0 = none): its CPU
    /// copies are skipped.
    window_id: AtomicU32,
    /// The fallback has been logged for this stream.
    logged: AtomicBool,
}

/// A tapped capture frame: a copy of the share's frame in an encoder-owned
/// `CVPixelBuffer`.
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub struct TapFrame {
    pub buffer: super::macos_encode::PixelBuffer,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Default)]
pub struct LiveTap {
    /// 0 = unbound (window ids are never 0).
    window_id: AtomicU32,
    /// There is a frame to stream: the bound share's, or, just after a switch,
    /// the previous share's last one until the new share delivers.
    share_up: AtomicBool,
    /// The bound share stopped: its window is refused until a rebind.
    ended: AtomicBool,
    /// Frames of the bound share dropped because they may still show Petal:
    /// the share is sending, just not provably without Petal yet.
    unexcluded_drops: std::sync::atomic::AtomicU64,
    pub mailbox: FrameMailbox<TapFrame>,
    #[cfg(target_os = "windows")]
    gpu: GpuTapState,
}

impl LiveTap {
    pub fn window_id(&self) -> Option<u32> {
        match self.window_id.load(Ordering::Acquire) {
            0 => None,
            id => Some(id),
        }
    }

    /// The bound share is publishing frames.
    pub fn share_available(&self) -> bool {
        self.share_up.load(Ordering::Acquire)
    }

    /// The bound share stopped and nothing has been resumed since.
    pub fn share_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    /// The bound share is sending frames, but none provably without Petal
    /// yet (a static share captured before the exclusion).
    pub fn only_unexcluded_frames(&self) -> bool {
        !self.share_available() && self.unexcluded_drops.load(Ordering::Acquire) > 0
    }

    /// Bind to `window_id`, the stopped share's own window included. A live
    /// switch keeps the previous share's last frame on screen until the new
    /// share delivers (a static window only re-pushes every couple of
    /// seconds), rather than flashing the slate.
    fn rebind(&self, window_id: u32) {
        self.window_id.store(window_id, Ordering::Release);
        self.ended.store(false, Ordering::Release);
        #[cfg(target_os = "windows")]
        {
            // The new share's capture proves its own adapter.
            self.gpu.window_id.store(0, Ordering::Release);
            self.gpu.capture_luid.store(0, Ordering::Release);
        }
    }
}

#[cfg(target_os = "windows")]
impl LiveTap {
    pub fn note_capture_adapter(&self, luid: i64) {
        self.gpu.capture_luid.store(luid, Ordering::Release);
    }

    /// The bound share's capture adapter, once a frame has arrived.
    pub fn capture_adapter(&self) -> Option<i64> {
        match self.gpu.capture_luid.load(Ordering::Acquire) {
            0 => None,
            luid => Some(luid),
        }
    }

    /// The encoder, on adapter `luid`, takes GPU frames from now on.
    pub fn accept_gpu(&self, luid: i64) {
        if !self.gpu.failed.load(Ordering::Acquire) {
            self.gpu.encoder_luid.store(luid, Ordering::Release);
        }
    }

    /// The adapter GPU frames must come from, while the encoder takes them.
    pub fn gpu_encoder_adapter(&self) -> Option<i64> {
        if self.gpu.failed.load(Ordering::Acquire) {
            return None;
        }
        match self.gpu.encoder_luid.load(Ordering::Acquire) {
            0 => None,
            luid => Some(luid),
        }
    }

    /// The (new) encoder can't take GPU frames: clear any adapter an earlier
    /// encoder accepted, so captures go back to CPU copies.
    pub fn decline_gpu(&self, reason: &str) {
        self.gpu.encoder_luid.store(0, Ordering::SeqCst);
        self.gpu_fell_back(reason);
    }

    /// A capture's GPU path failed: its frames take the CPU copy again.
    pub fn gpu_fell_back(&self, reason: &str) {
        self.gpu.window_id.store(0, Ordering::SeqCst);
        self.log_fallback_once(reason);
    }

    /// The encoder couldn't use a shared frame: CPU copies for the rest of
    /// the stream.
    pub fn disable_gpu(&self, reason: &str) {
        self.gpu.failed.store(true, Ordering::SeqCst);
        self.gpu.encoder_luid.store(0, Ordering::SeqCst);
        self.gpu.window_id.store(0, Ordering::SeqCst);
        self.log_fallback_once(reason);
    }

    pub fn gpu_disabled(&self) -> bool {
        self.gpu.failed.load(Ordering::Acquire)
    }

    fn log_fallback_once(&self, reason: &str) {
        if !self.gpu.logged.swap(true, Ordering::AcqRel) {
            log::warn!("streaming: GPU frame tap off, using the CPU copy: {reason}");
        }
    }
}

fn slot() -> &'static Mutex<Option<Arc<LiveTap>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<LiveTap>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Fast-path flag so idle capture callbacks never take the lock.
static ANY_LIVE: AtomicBool = AtomicBool::new(false);

/// Start tapping `window_id`. Replaces any previous tap.
pub fn start(window_id: u32) -> Arc<LiveTap> {
    let tap = Arc::new(LiveTap::default());
    tap.rebind(window_id);
    *slot().lock_unpoisoned() = Some(tap.clone());
    ANY_LIVE.store(true, Ordering::Release);
    tap
}

pub fn stop() {
    ANY_LIVE.store(false, Ordering::Release);
    *slot().lock_unpoisoned() = None;
}

fn current() -> Option<Arc<LiveTap>> {
    if !ANY_LIVE.load(Ordering::Acquire) {
        return None;
    }
    slot().lock_unpoisoned().clone()
}

/// "Resume/Switch live stream here": move the tap to another of the sharer's
/// shares.
pub fn rebind(window_id: u32) -> bool {
    match current() {
        Some(tap) => {
            tap.rebind(window_id);
            true
        }
        None => false,
    }
}

/// Frames of the bound share dropped because they may still show Petal.
static UNEXCLUDED_DROPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Called from every share's capture callback. `copy` runs only when this
/// share is the bound one and the frame leaves Petal's own windows out
/// (`excludes_petal`); a frame it can't copy is skipped.
pub fn offer_frame(window_id: u32, excludes_petal: bool, copy: impl FnOnce() -> Option<TapFrame>) {
    let Some(tap) = current() else { return };
    if tap.window_id.load(Ordering::Acquire) != window_id || tap.share_ended() {
        return;
    }
    if !excludes_petal {
        tap.unexcluded_drops.fetch_add(1, Ordering::AcqRel);
        note_unexcluded_drop(window_id);
        return;
    }
    #[cfg(target_os = "windows")]
    if tap.gpu.window_id.load(Ordering::Acquire) == window_id {
        return;
    }
    let Some(frame) = copy() else { return };
    tap.mailbox.put(frame);
    tap.share_up.store(true, Ordering::Release);
}

/// The live tap, if share `window_id` is the one streaming: the capture
/// thread's GPU path asks before it copies anything.
#[cfg(target_os = "windows")]
pub fn gpu_target(window_id: u32) -> Option<Arc<LiveTap>> {
    current().filter(|tap| tap.window_id.load(Ordering::Acquire) == window_id && !tap.share_ended())
}

/// A GPU frame from share `window_id`'s capture thread. From now on its CPU
/// copies are skipped.
#[cfg(target_os = "windows")]
pub fn offer_gpu_frame(tap: &LiveTap, window_id: u32, excludes_petal: bool, frame: super::gpu_tap::GpuFrame) {
    if tap.window_id.load(Ordering::Acquire) != window_id || tap.share_ended() {
        return;
    }
    if !excludes_petal {
        tap.unexcluded_drops.fetch_add(1, Ordering::AcqRel);
        note_unexcluded_drop(window_id);
        return;
    }
    let (width, height) = frame.size();
    tap.mailbox.put(TapFrame { pixels: TapPixels::Gpu(frame), width, height });
    tap.gpu.window_id.store(window_id, Ordering::SeqCst);
    // The encoder may have declined GPU frames since this capture checked
    // (a device recreated on the CPU path): don't leave its CPU copies off.
    if tap.gpu.encoder_luid.load(Ordering::SeqCst) == 0 || tap.gpu.failed.load(Ordering::SeqCst) {
        tap.gpu.window_id.store(0, Ordering::SeqCst);
    }
    tap.share_up.store(true, Ordering::Release);
}

fn note_unexcluded_drop(window_id: u32) {
    let dropped = UNEXCLUDED_DROPS.fetch_add(1, Ordering::Relaxed);
    if dropped % 300 == 0 {
        log::info!("streaming: dropped share {window_id}'s frame taken before Petal was excluded ({} so far)", dropped + 1);
    }
}

/// Called when any share stops (user, window closed, failure). The stream
/// falls back to the slate until the sharer resumes on a share.
pub fn share_ended(window_id: u32) {
    let Some(tap) = current() else { return };
    if tap.window_id.load(Ordering::Acquire) == window_id {
        tap.ended.store(true, Ordering::Release);
        tap.share_up.store(false, Ordering::Release);
        // Releases any GPU slot the mailbox held.
        tap.mailbox.clear();
        #[cfg(target_os = "windows")]
        tap.gpu.window_id.store(0, Ordering::Release);
    }
}

/// Whether the live share has stopped (the stream is waiting for a Resume).
pub fn live_share_ended() -> bool {
    current().is_some_and(|tap| tap.share_ended())
}

/// The share streaming right now, if any (not one that stopped). While live
/// it stays at full quality: exempt from macOS's focus-weighted `Reduced`
/// demotion.
pub fn live_window_id() -> Option<u32> {
    current().filter(|tap| !tap.share_ended()).and_then(|tap| tap.window_id())
}

pub fn is_live_window(window_id: u32) -> bool {
    live_window_id() == Some(window_id)
}

#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock_unpoisoned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "macos"))]
    fn frame(width: u32) -> Option<TapFrame> {
        let pixels = TapPixels::Bgra { bgra: vec![0; (width * 4) as usize], bytes_per_row: width * 4 };
        Some(TapFrame { pixels, width, height: 1 })
    }

    #[cfg(target_os = "macos")]
    fn frame(width: u32) -> Option<TapFrame> {
        let bgra = vec![0; (width * 4) as usize];
        let buffer = super::super::macos_encode::PixelBuffer::from_bgra(&bgra, width * 4, width, 1).unwrap();
        Some(TapFrame { buffer, width, height: 1 })
    }

    #[test]
    fn only_the_bound_share_is_copied() {
        let _guard = test_lock();
        let tap = start(7);
        let mut copied = false;
        offer_frame(8, true, || {
            copied = true;
            frame(1)
        });
        assert!(!copied, "another share's frames are never copied");
        assert!(!tap.share_available());
        offer_frame(7, true, || frame(4));
        assert!(tap.share_available());
        assert_eq!(tap.mailbox.latest().unwrap().0.width, 4);
        stop();
        offer_frame(7, true, || panic!("nothing live: never copied"));
    }

    #[test]
    fn no_frame_reaches_the_stream_before_petal_is_excluded() {
        let _guard = test_lock();
        let tap = start(7);
        // Taken before the exclusion took effect: never copied, never queued.
        offer_frame(7, false, || panic!("a frame that may show Petal is never copied"));
        assert!(!tap.share_available());
        assert!(tap.mailbox.latest().is_none());
        assert!(tap.only_unexcluded_frames(), "go-live can tell a static share from a silent one");
        // The first frame after it is the first frame of the stream.
        offer_frame(7, true, || frame(4));
        assert!(tap.share_available());
        assert!(!tap.only_unexcluded_frames());
        assert_eq!(tap.mailbox.latest().unwrap().0.width, 4);
        // A late pre-exclusion frame (a parked re-push) still can't replace it.
        offer_frame(7, false, || panic!("never copied"));
        assert_eq!(tap.mailbox.latest().unwrap().0.width, 4);
        stop();
    }

    #[test]
    fn a_stopped_share_never_comes_back_on_air_by_itself() {
        let _guard = test_lock();
        let tap = start(7);
        offer_frame(7, true, || frame(4));
        share_ended(8);
        assert!(tap.share_available(), "another share ending changes nothing");
        assert!(!live_share_ended());
        share_ended(7);
        assert!(!tap.share_available());
        assert!(live_share_ended());
        assert!(tap.mailbox.latest().is_none(), "no stale frame behind the slate");
        // The same window shared again (macOS keeps its id): still refused.
        offer_frame(7, true, || panic!("a stopped share is never tapped again by itself"));
        assert!(!tap.share_available());
        // Until the sharer resumes on it.
        assert!(rebind(7));
        assert!(!live_share_ended());
        offer_frame(7, true, || frame(6));
        assert!(tap.share_available());
        stop();
    }

    #[test]
    fn resume_here_moves_the_binding_and_exempts_the_new_share() {
        let _guard = test_lock();
        let tap = start(7);
        assert!(is_live_window(7));
        share_ended(7);
        assert!(rebind(9));
        assert!(is_live_window(9) && !is_live_window(7));
        offer_frame(7, true, || panic!("old share no longer tapped"));
        offer_frame(9, true, || frame(2));
        assert!(tap.share_available());
        stop();
        assert_eq!(live_window_id(), None);
        assert!(!rebind(9));
    }

    #[test]
    fn switching_while_live_holds_the_last_frame_until_the_new_share_delivers() {
        let _guard = test_lock();
        let tap = start(7);
        offer_frame(7, true, || frame(4));
        assert!(rebind(9));
        // No slate flash: the old share's last frame stays up.
        assert!(tap.share_available());
        assert_eq!(tap.mailbox.latest().unwrap().0.width, 4);
        offer_frame(7, true, || panic!("old share no longer tapped"));
        offer_frame(9, true, || frame(6));
        assert_eq!(tap.mailbox.latest().unwrap().0.width, 6);
        // If the new share ends before delivering, the slate takes over.
        assert!(rebind(7));
        share_ended(7);
        assert!(!tap.share_available() && tap.mailbox.latest().is_none());
        stop();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn the_gpu_handshake_gates_the_capture_path() {
        let _guard = test_lock();
        let tap = start(7);
        // Nothing accepted yet: the capture only reports its adapter.
        assert!(gpu_target(7).is_some() && gpu_target(8).is_none());
        assert_eq!(tap.gpu_encoder_adapter(), None);
        tap.note_capture_adapter(42);
        assert_eq!(tap.capture_adapter(), Some(42));
        tap.accept_gpu(42);
        assert_eq!(tap.gpu_encoder_adapter(), Some(42));
        // A capture that couldn't share frames goes back to CPU copies.
        tap.gpu.window_id.store(7, Ordering::Release);
        offer_frame(7, true, || panic!("GPU frames are flowing: no CPU copy"));
        tap.gpu_fell_back("test");
        offer_frame(7, true, || frame(4));
        assert_eq!(tap.mailbox.latest().unwrap().0.width, 4);
        // The encoder giving up ends GPU frames for the stream.
        tap.disable_gpu("test");
        tap.accept_gpu(42);
        assert_eq!(tap.gpu_encoder_adapter(), None);
        // A rebind forgets the old capture's adapter.
        assert!(rebind(9));
        assert_eq!(tap.capture_adapter(), None);
        stop();
    }
}
