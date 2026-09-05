//! Single source of truth for window state (#744, Phase 2).
//!
//! Replaces the ~95+/s of independent `CGWindowListCopyWindowInfo`
//! enumerations (plan §1) with ONE snapshot that every consumer reads. This
//! commit lands the CG-backed (T2) ingest tier and the read API; the AX (T1)
//! and SkyLight (T0) event feeds are #747/#748. The gesture fast path and the
//! remaining consumer migrations follow in later Phase-2 commits.
//!
//! Design: `internal/docs/WINDOW_REGISTRY_PLAN.md` §3. Key rules honored here:
//! - **Never drop windows at ingest** (§9.8/§9.9). The snapshot carries every
//!   window raw; classification is a derived per-record label, and consumers
//!   apply their own policy via `window_policy`.
//! - The store is published behind `RwLock<Arc<Snapshot>>` so 60Hz readers
//!   clone an `Arc` cheaply and never block the writer for long. (`arc-swap`
//!   would shave the read further; deferred to avoid a new dependency.)
//! - macOS-only ingest; the types compile everywhere so Windows/tests can use
//!   the read API against an injected snapshot.

use crate::platform::cg::WindowFrame;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

/// Derived classification of a window (plan §3 "Classification & policy").
/// Only the STAGE-0 variants — decidable from snapshot data with no AX round
/// trip — are produced in this commit. `PetalOwned` carries the
/// `Decorative|Content` subtype the hover hit-test needs (§9.9): Petal renders
/// remote shared windows as its own panels that must BLOCK the hit-test, so
/// "ours" is not enough to know whether to skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowClass {
    /// layer != 0: Dock, menu bar, overlays. No AX needed.
    SystemChrome,
    /// Owned by our own pid. `decorative` = a share-border/overlay/hover panel
    /// the hover hit-test SKIPS; otherwise (main window, remote renders) BLOCK.
    PetalOwned { decorative: bool },
    /// A persistent hollow Petal View selector.
    RegionSelector,
    /// A normal foreign window. Stage 1 (AX, #747) will refine this into
    /// Standard/Dialog/Popup; until then every non-chrome foreign window is
    /// `Unknown`-with-`is_real` computed from geometry.
    Unknown,
}

/// One window in the snapshot. Carries the RAW CoreGraphics fields (never
/// filtered) plus the derived class. `name`/`owner_name` may be empty — the
/// lean ingest path drops them (§9.2) since classification does not need them.
#[derive(Debug, Clone)]
pub struct WindowRecord {
    pub wid: u32,
    /// Truncated (as-i32) frame — matches `onscreen_stack()` / share_border, so
    /// migrated consumers stay byte-identical. Derive other precisions from the
    /// raw fields below; NEVER re-round a consumer silently.
    pub frame: WindowFrame,
    /// RAW geometry (plan §9.8/§9.10 "carry raw fields, never normalize").
    /// occlusion computes areas in f64; hover rounds. Keeping the raw values
    /// lets every consumer apply its own conversion without divergence.
    pub rx: f64,
    pub ry: f64,
    pub rw: f64,
    pub rh: f64,
    pub layer: i64,
    pub alpha: f64,
    pub owner_pid: i32,
    pub class: WindowClass,
    /// Geometry-only "looks like a real user window" flag (layer 0, opaque,
    /// >= 40pt). NOT a policy decision — each consumer's `window_policy` view
    /// decides what it wants; this is a convenience the cheap classifiers use.
    pub is_real: bool,
}

/// Immutable published state. Readers hold an `Arc<Snapshot>`; the ingest
/// thread swaps a fresh one in. Front-to-back order matches the CG list.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// wid -> record.
    pub by_id: HashMap<u32, WindowRecord>,
    /// Front-to-back window ids (index 0 = frontmost), the CG list order.
    pub order: Vec<u32>,
    /// Bumped whenever `order` changes (z-order), for cheap change detection.
    pub order_gen: u64,
    /// Bumped on every published snapshot.
    pub gen: u64,
}

impl Snapshot {
    pub fn frame(&self, wid: u32) -> Option<WindowFrame> {
        self.by_id.get(&wid).map(|r| r.frame)
    }
    pub fn exists(&self, wid: u32) -> bool {
        self.by_id.contains_key(&wid)
    }
    pub fn owner_pid(&self, wid: u32) -> Option<i32> {
        self.by_id.get(&wid).map(|r| r.owner_pid)
    }
    /// Front-to-back records in `order`.
    pub fn records_front_to_back(&self) -> impl Iterator<Item = &WindowRecord> {
        self.order.iter().filter_map(move |wid| self.by_id.get(wid))
    }
}

/// The `is_real` geometry predicate (plan §3 stage 0). Deliberately matches the
/// most common existing filter (layer 0, alpha >= 0.99, >= 40pt); consumers
/// that want a different rule keep their own (that is what `window_policy` is
/// for). NOT used to drop records — only to label them.
fn geometry_is_real(layer: i64, alpha: f64, w: f64, h: f64) -> bool {
    layer == 0 && alpha >= 0.99 && w >= 40.0 && h >= 40.0
}

/// Stage-0 classification from snapshot data alone.
fn classify_stage0(
    layer: i64,
    owner_pid: i32,
    self_pid: i32,
    is_decorative_own: bool,
    is_region_selector: bool,
) -> WindowClass {
    // A source registered from ScreenCaptureKit (or from the native selector
    // handle at creation) is an authenticated Petal View identity. It must
    // win before the parallel CG owner/layer fields: macOS VMs can report a
    // stale owner PID or floating layer for the same window. The fallback
    // title-plus-current-Petal-owner path still sets `is_region_selector` only
    // for the Windows-equivalent ownership rule.
    if is_region_selector {
        WindowClass::RegionSelector
    } else if owner_pid == self_pid {
        WindowClass::PetalOwned {
            decorative: is_decorative_own,
        }
    } else if layer != 0 {
        WindowClass::SystemChrome
    } else {
        WindowClass::Unknown
    }
}

/// How the ingest decides whether one of OUR windows is decorative chrome
/// (skip) vs content (block). Injected so tests are deterministic and so the
/// eventual `NSApp.windows`/panel-class source (§9.9) can replace it without
/// touching ingest. For now it is name-based, matching the hover hit-test's
/// current own-chrome check exactly (behavior parity, pinned by the goldens).
pub trait OwnChromeOracle: Send + Sync {
    fn is_decorative(&self, window_name: &str) -> bool;
}

/// Registry handle. Cloneable; all clones share one store.
#[derive(Clone)]
pub struct WindowRegistry {
    inner: Arc<Inner>,
}

/// Max per-window misses (readable AX array, window absent) before settling
/// as `None`. Retries happen one ingest tick apart (~100ms in-room), so this
/// covers about a second of AX-registration lag after window creation (#747).
pub const AX_KIND_MAX_ATTEMPTS: u8 = 10;

/// Max APP-level AX failures for one pid before it is marked `AXDead` and
/// stage-1 stops attempting any of its windows (§3; each failed app-level
/// call can block the ingest tick up to 250ms — never pay that per window).
pub const AX_APP_MAX_FAILURES: u8 = 3;

/// AX subrole classification for a window (#747 stage 1). Mirrors
/// `platform::ax::AxKind` but is defined here so the registry API compiles on
/// every platform. Resolved once per window lifetime, cached, pruned on
/// destroy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxKind {
    Standard,
    Dialog,
    Popup,
}

/// Outcome of one injected per-app resolution pass (see
/// [`WindowRegistry::ensure_kinds_resolved_for_app`]). Platform-neutral mirror
/// of `platform::ax::AppKindsOutcome`.
pub enum AppKindsResult {
    Resolved(HashMap<u32, AxKind>),
    AppUnavailable,
}

struct Inner {
    snapshot: RwLock<Arc<Snapshot>>,
    /// Ids consumers care about (hovered/shared/bordered) — reserved for the
    /// gesture fast path (#744 later commit). Kept now so the API is stable.
    hot: Mutex<Vec<u32>>,
    /// wid -> resolved AX kind. Presence means resolution SETTLED (value
    /// `None` = gave up after the retry budget), so consumers read a cache,
    /// never trigger AX (§3 stage-1 budget). Pruned in `publish` when a wid
    /// leaves the snapshot — the "invalidated on destroy" rule, via the audit
    /// sweep since SLS destroy events are Phase 4.
    ax_kinds: Mutex<HashMap<u32, Option<AxKind>>>,
    /// wid -> miss count for windows NOT yet settled: the app's AX array was
    /// readable but this window wasn't in it yet. Retried (next ingest tick,
    /// ~100ms apart) up to [`AX_KIND_MAX_ATTEMPTS`] before settling as
    /// permanent `None`, because a freshly-created window takes a beat to
    /// appear in its app's AX windows array — resolve-once-at-first-sight
    /// permanently mis-cached every window BORN while in a room (#747 live
    /// finding). Successful resolutions still settle on first hit.
    ax_pending: Mutex<HashMap<u32, u8>>,
    /// pid -> APP-level AX failure strikes (no AX server / API disabled /
    /// timeout on the `kAXWindows` copy itself). At
    /// [`AX_APP_MAX_FAILURES`] the pid is `AXDead` (§3): stage-1 skips every
    /// window of that app — no per-window retries against an app that can
    /// only time out (each blocked attempt costs up to 250ms). Cleared when
    /// the pid no longer owns any snapshot window (app quit/relaunch).
    ax_dead: Mutex<HashMap<i32, u8>>,
    gen: std::sync::atomic::AtomicU64,
}

impl Default for WindowRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                snapshot: RwLock::new(Arc::new(Snapshot::default())),
                hot: Mutex::new(Vec::new()),
                ax_kinds: Mutex::new(HashMap::new()),
                ax_pending: Mutex::new(HashMap::new()),
                ax_dead: Mutex::new(HashMap::new()),
                gen: std::sync::atomic::AtomicU64::new(0),
            }),
        }
    }

    /// Lock-light read: clone the current `Arc<Snapshot>`.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.inner
            .snapshot
            .read()
            .expect("registry snapshot lock poisoned")
            .clone()
    }

    pub fn frame(&self, wid: u32) -> Option<WindowFrame> {
        self.snapshot().frame(wid)
    }
    pub fn exists(&self, wid: u32) -> bool {
        self.snapshot().exists(wid)
    }
    pub fn owner_pid(&self, wid: u32) -> Option<i32> {
        self.snapshot().owner_pid(wid)
    }
    pub fn order_generation(&self) -> u64 {
        self.snapshot().order_gen
    }

    /// Force a fresh CG sweep NOW and return the updated snapshot, for
    /// consumers that need per-interaction truth fresher than the ~10Hz ingest
    /// (plan §5 — remote_control's blocking hit-test). No-op off macOS.
    pub fn refresh_now(&self) -> Arc<Snapshot> {
        #[cfg(target_os = "macos")]
        ingest::refresh_once(self);
        self.snapshot()
    }

    /// Follow-freshness read (#747 §4): when the gesture fast path is
    /// actively tracking `followed` (fresh per-id frame published within
    /// 50ms), the snapshot is already at least as fresh as a sweep would
    /// make it FOR THAT WINDOW — return it without paying a full
    /// enumeration. Every other case (no gesture, different window, tap
    /// unavailable) behaves exactly like [`Self::refresh_now`].
    pub fn refresh_for_follow(&self, _followed: Option<u32>) -> Arc<Snapshot> {
        #[cfg(target_os = "macos")]
        {
            if let Some(wid) = _followed {
                if crate::platform::gesture_tap::gesture_fresh_for(wid, 50) {
                    return self.snapshot();
                }
            }
        }
        self.refresh_now()
    }

    /// Publish an updated RAW frame for one window (gesture fast path, §4).
    /// Order is unchanged (a drag does not reorder mid-gesture; the drag-end
    /// sweep reconciles), so `order_gen` stays put and only `gen` bumps.
    pub fn update_window_frame(&self, wid: u32, x: f64, y: f64, w: f64, h: f64) {
        let prev = self.snapshot();
        if !prev.by_id.contains_key(&wid) {
            return;
        }
        let mut snap = (*prev).clone();
        if let Some(r) = snap.by_id.get_mut(&wid) {
            r.rx = x;
            r.ry = y;
            r.rw = w;
            r.rh = h;
            r.frame = WindowFrame {
                x: x.round() as i32,
                y: y.round() as i32,
                width: w.round() as i32,
                height: h.round() as i32,
            };
        }
        self.publish(snap);
    }

    /// Topmost foreign layer-0 window containing the point (gesture targeting,
    /// §4). Deliberately simpler than hover's policy: any foreign layer-0
    /// window can be dragged, so no size/denylist filtering here.
    pub fn topmost_foreign_at(&self, x: f64, y: f64, self_pid: i32) -> Option<u32> {
        let snap = self.snapshot();
        let hit = snap
            .records_front_to_back()
            .find(|r| {
                r.layer == 0
                    && r.owner_pid != self_pid
                    && x >= r.rx
                    && x < r.rx + r.rw
                    && y >= r.ry
                    && y < r.ry + r.rh
            })
            .map(|r| r.wid);
        hit
    }

    /// A FRESH single-window frame via the cheap per-id CG query (Phase 1's
    /// `CGWindowListCreateDescriptionFromArray`, ~65us), NOT the ~10Hz snapshot.
    /// For event-driven one-shots that want current truth (shortcuts re-share,
    /// display-reconfig repair) while still routing through the registry so no
    /// consumer reaches into `platform::cg` directly.
    pub fn frame_fresh(&self, wid: u32) -> Option<WindowFrame> {
        crate::platform::cg::frame_for_window_id(wid)
    }

    /// A FRESH owning-pid for a window via the cheap per-id CG query (OptionAll
    /// semantics: resolves minimized / other-Space windows too). For share-start
    /// focus, remote-control target resolution, ai_chat -- event-driven callers
    /// that want current truth, routed through the registry.
    pub fn owner_pid_fresh(&self, wid: u32) -> Option<i32> {
        crate::platform::cg::owner_pid_for_window_id(wid)
    }

    /// Fresh single-id existence (OptionAll). Routes `session/share`'s
    /// `WindowExistence` and any other existence check through the registry.
    pub fn exists_fresh(&self, wid: u32) -> bool {
        crate::platform::cg::window_exists(wid)
    }

    /// The cached AX kind for a window (`None` if not yet resolved or
    /// unavailable). A pure cache READ — never triggers AX, so consumers may
    /// call it on their hot path (§3: "none from consumer read paths").
    pub fn window_kind(&self, wid: u32) -> Option<AxKind> {
        self.inner
            .ax_kinds
            .lock()
            .expect("ax_kinds lock poisoned")
            .get(&wid)
            .copied()
            .flatten()
    }

    /// Batch outcome type the injected per-app resolver returns. Mirrors
    /// `platform::ax::AppKindsOutcome` but is defined here so the registry API
    /// (and its tests) compile on every platform.
    ///
    /// `Resolved`: the app's AX array was readable; the map holds kinds for
    /// the wanted wids that were present. `AppUnavailable`: the app-LEVEL
    /// query failed (no AX server / disabled / timeout).
    pub fn ensure_kinds_resolved_for_app(
        &self,
        pid: i32,
        wanted: &[(u32, (f64, f64, f64, f64))],
        resolver: impl FnOnce(i32, &HashMap<u32, (f64, f64, f64, f64)>) -> AppKindsResult,
    ) {
        // Drop already-settled wids; skip entirely for a dead pid.
        if self.ax_pid_dead(pid) {
            return;
        }
        // wid -> raw CG frame: the id key for `_AXUIElementGetWindow` mode and
        // the match target for the (pid,frame) correlation fallback (§3).
        let wanted: HashMap<u32, (f64, f64, f64, f64)> = {
            let cache = self.inner.ax_kinds.lock().expect("ax_kinds lock poisoned");
            wanted
                .iter()
                .copied()
                .filter(|(w, _)| !cache.contains_key(w))
                .collect()
        };
        if wanted.is_empty() {
            return;
        }
        match resolver(pid, &wanted) {
            AppKindsResult::Resolved(kinds) => {
                // App answered: clear its strike count.
                self.inner
                    .ax_dead
                    .lock()
                    .expect("ax_dead lock poisoned")
                    .remove(&pid);
                let mut cache = self.inner.ax_kinds.lock().expect("ax_kinds lock poisoned");
                let mut pending = self
                    .inner
                    .ax_pending
                    .lock()
                    .expect("ax_pending lock poisoned");
                for wid in wanted.into_keys() {
                    if let Some(kind) = kinds.get(&wid) {
                        // Winning resolution: settles on FIRST hit (≤1 winning
                        // AX pass per window lifetime, §3 budget).
                        pending.remove(&wid);
                        cache.insert(wid, Some(*kind));
                    } else {
                        // Readable array but window absent: birth lag (#747).
                        // Retry next tick, bounded.
                        let misses = pending.entry(wid).or_insert(0);
                        *misses += 1;
                        if *misses >= AX_KIND_MAX_ATTEMPTS {
                            pending.remove(&wid);
                            cache.insert(wid, None);
                            log::info!(
                                "winsrv: window {wid} absent from its app's AX array after {AX_KIND_MAX_ATTEMPTS} looks -- keeping stage-0 class"
                            );
                        }
                    }
                }
            }
            AppKindsResult::AppUnavailable => {
                let mut dead = self.inner.ax_dead.lock().expect("ax_dead lock poisoned");
                let strikes = dead.entry(pid).or_insert(0);
                *strikes += 1;
                if *strikes == AX_APP_MAX_FAILURES {
                    log::info!(
                        "winsrv: pid {pid} AX-dead after {AX_APP_MAX_FAILURES} app-level failures -- stage-1 disabled for its windows (cleared on app exit)"
                    );
                }
            }
        }
    }

    /// Whether a stage-1 classification for `wid` is GENUINELY still in
    /// flight: stage-1 is live (canary passed), the window hasn't settled,
    /// and its app isn't `AXDead`. Hover DEFERS the pill while this is true
    /// (#747 audit follow-up): moving resolution to the 10Hz ingest thread
    /// means a fresh popup's classification can land ~100ms after the window
    /// appears, and "unsettled ⇒ show" flashed the pill in that gap (caught
    /// by the live popup gate: three `show` lines 45ms before `classified …
    /// as Popup`). Bounded: settles in one pass typically, worst ~1s via the
    /// miss budget — and returns false outright on degraded/T2 rigs and dead
    /// apps so those keep instant pills.
    pub fn kind_resolution_in_flight(&self, wid: u32, owner_pid: i32) -> bool {
        #[cfg(target_os = "macos")]
        {
            ingest::ax_canary_ok() && !self.kind_settled(wid) && !self.ax_pid_dead(owner_pid)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (wid, owner_pid);
            false
        }
    }

    /// Re-resolve SETTLED kinds after an AX state event (§3: AXSubrole
    /// mutates with window state — Standard ↔ Dialog ↔ Floating). Unlike
    /// [`Self::ensure_kinds_resolved_for_app`], this bypasses the settled
    /// filter, REPLACES the cached value only on a successful resolution, and
    /// never unsettles, counts misses, or strikes the pid — so consumers
    /// (hover's pill-defer gate included) keep the old value with zero
    /// flicker until the new one lands, and a failed recheck costs nothing.
    pub fn recheck_kinds_for_app(
        &self,
        pid: i32,
        wanted: &[(u32, (f64, f64, f64, f64))],
        resolver: impl FnOnce(i32, &HashMap<u32, (f64, f64, f64, f64)>) -> AppKindsResult,
    ) {
        if self.ax_pid_dead(pid) || wanted.is_empty() {
            return;
        }
        let wanted: HashMap<u32, (f64, f64, f64, f64)> = wanted.iter().copied().collect();
        if let AppKindsResult::Resolved(kinds) = resolver(pid, &wanted) {
            let mut cache = self.inner.ax_kinds.lock().expect("ax_kinds lock poisoned");
            for (wid, kind) in kinds {
                if let Some(old) = cache.insert(wid, Some(kind)) {
                    if old != Some(kind) {
                        log::info!(
                            "winsrv: window {wid} subrole changed {old:?} -> {kind:?} (state-event recheck)"
                        );
                    }
                }
            }
        }
    }

    /// Whether stage-1 has written off this pid's AX server (§3 `AXDead`).
    pub fn ax_pid_dead(&self, pid: i32) -> bool {
        self.inner
            .ax_dead
            .lock()
            .expect("ax_dead lock poisoned")
            .get(&pid)
            .is_some_and(|&s| s >= AX_APP_MAX_FAILURES)
    }

    /// Whether stage-1 resolution has SETTLED for `wid` (either kind or a
    /// final give-up). Unsettled windows are still being retried by ingest.
    pub fn kind_settled(&self, wid: u32) -> bool {
        self.inner
            .ax_kinds
            .lock()
            .expect("ax_kinds lock poisoned")
            .contains_key(&wid)
    }

    pub fn mark_hot(&self, wid: u32) {
        let mut hot = self.inner.hot.lock().expect("hot lock poisoned");
        if !hot.contains(&wid) {
            hot.push(wid);
        }
    }
    pub fn clear_hot(&self, wid: u32) {
        self.inner
            .hot
            .lock()
            .expect("hot lock poisoned")
            .retain(|&w| w != wid);
    }

    /// Publish a freshly built snapshot, assigning `gen` and computing
    /// `order_gen` (bumped only when the order actually changed).
    fn publish(&self, mut snap: Snapshot) {
        let prev = self.snapshot();
        let gen = self
            .inner
            .gen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        snap.gen = gen;
        snap.order_gen = if snap.order == prev.order {
            prev.order_gen
        } else {
            prev.order_gen + 1
        };
        // Prune AX-kind cache entries for windows that left the snapshot: the
        // "invalidate on destroy" rule (§3), enforced via the audit sweep until
        // SLS destroy events land (#748). A window id that reappears re-resolves.
        {
            let mut cache = self.inner.ax_kinds.lock().expect("ax_kinds lock poisoned");
            if !cache.is_empty() {
                cache.retain(|wid, _| snap.by_id.contains_key(wid));
            }
        }
        {
            let mut pending = self
                .inner
                .ax_pending
                .lock()
                .expect("ax_pending lock poisoned");
            if !pending.is_empty() {
                pending.retain(|wid, _| snap.by_id.contains_key(wid));
            }
        }
        {
            // AXDead clears when the app no longer owns any snapshot window
            // (quit/relaunch -- §3 "cleared on app relaunch").
            let mut dead = self.inner.ax_dead.lock().expect("ax_dead lock poisoned");
            if !dead.is_empty() {
                let live_pids: std::collections::HashSet<i32> =
                    snap.by_id.values().map(|r| r.owner_pid).collect();
                dead.retain(|pid, _| live_pids.contains(pid));
            }
        }
        *self
            .inner
            .snapshot
            .write()
            .expect("registry snapshot lock poisoned") = Arc::new(snap);
    }

    /// Build a snapshot from raw window rows (front-to-back). Shared by the CG
    /// ingest and the fixture-ingest test path so both produce byte-identical
    /// snapshots (the golden-transfer guarantee). `rows` is
    /// `(wid, frame, layer, alpha, owner_pid, name)` in front-to-back order.
    #[allow(clippy::type_complexity)]
    pub fn ingest_rows(
        &self,
        rows: &[(u32, f64, f64, f64, f64, i64, f64, i32, String)],
        self_pid: i32,
        chrome: &dyn OwnChromeOracle,
    ) {
        let mut by_id = HashMap::with_capacity(rows.len());
        let mut order = Vec::with_capacity(rows.len());
        for (wid, x, y, w, h, layer, alpha, owner_pid, name) in rows {
            let decorative = *owner_pid == self_pid && chrome.is_decorative(name);
            // A source registered by ScreenCaptureKit enumeration is an
            // authoritative Petal View identity. Keep that exception even if
            // the parallel CG row has a missing/stale owner PID (common in
            // VM/window-server edge cases).
            let is_region_selector = crate::region_window::resolve(*wid).is_some()
                || (*owner_pid == self_pid && crate::region_window::is_region_window_title(name));
            let class =
                classify_stage0(*layer, *owner_pid, self_pid, decorative, is_region_selector);
            let is_real = geometry_is_real(*layer, *alpha, *w, *h);
            by_id.insert(
                *wid,
                WindowRecord {
                    wid: *wid,
                    // Truncate to match onscreen_stack()/share_border exactly.
                    frame: WindowFrame {
                        x: *x as i32,
                        y: *y as i32,
                        width: *w as i32,
                        height: *h as i32,
                    },
                    rx: *x,
                    ry: *y,
                    rw: *w,
                    rh: *h,
                    layer: *layer,
                    alpha: *alpha,
                    owner_pid: *owner_pid,
                    class,
                    is_real,
                },
            );
            order.push(*wid);
        }
        self.publish(Snapshot {
            by_id,
            order,
            order_gen: 0,
            gen: 0,
        });
    }

    /// Fraction of `wid` covered by opaque, normal-level, foreign windows in
    /// front of it — the registry-snapshot equivalent of
    /// `cg::occlusion_fraction`, computed over the RAW f64 geometry so it is
    /// numerically identical. `None` if `wid` is not in the snapshot.
    pub fn occlusion(&self, wid: u32, self_pid: i32) -> Option<f64> {
        let snap = self.snapshot();
        let target_idx = snap.order.iter().position(|&w| w == wid)?;
        let target = snap.by_id.get(&wid)?;
        let target_area = target.rw * target.rh;
        if target_area <= 0.0 {
            return Some(0.0);
        }
        let (tx0, ty0, tx1, ty1) = (
            target.rx,
            target.ry,
            target.rx + target.rw,
            target.ry + target.rh,
        );
        let mut covered = 0.0_f64;
        for front_wid in &snap.order[..target_idx] {
            let Some(front) = snap.by_id.get(front_wid) else {
                continue;
            };
            if front.layer != 0 || front.alpha < 0.99 || front.owner_pid == self_pid {
                continue;
            }
            let left = front.rx.max(tx0);
            let top = front.ry.max(ty0);
            let right = (front.rx + front.rw).min(tx1);
            let bottom = (front.ry + front.rh).min(ty1);
            covered += (right - left).max(0.0) * (bottom - top).max(0.0);
        }
        Some((covered / target_area).clamp(0.0, 1.0))
    }
}

/// Process-global handle to the one registry. Set once at startup
/// (`set_global`). Lets consumers WITHOUT an `AppHandle` — capture callbacks,
/// tight background loops — read the snapshot. The registry is a genuine
/// singleton (one window server, one snapshot), so a global is appropriate
/// here where threading a handle through every signature is not.
static GLOBAL: std::sync::OnceLock<WindowRegistry> = std::sync::OnceLock::new();

pub fn set_global(registry: WindowRegistry) {
    let _ = GLOBAL.set(registry);
}

/// The global registry, or `None` before startup wiring (tests, early boot).
pub fn global() -> Option<&'static WindowRegistry> {
    GLOBAL.get()
}

/// macOS CG-backed ingest (T2). One lean enumeration per refresh, published to
/// the registry. Runs on its own thread while `active` is set.
#[cfg(target_os = "macos")]
pub mod ingest {
    use super::*;

    /// Name-based own-chrome oracle matching the hover hit-test's current
    /// behavior exactly (§9.9 option 1 / behavior parity). Replaced by an
    /// `NSApp.windows`-backed subtype source in a later commit.
    pub struct NameChromeOracle;
    impl OwnChromeOracle for NameChromeOracle {
        fn is_decorative(&self, window_name: &str) -> bool {
            window_name == crate::share_border::SHARE_BORDER_WINDOW_TITLE
                || window_name == crate::share_overlay::SHARE_OVERLAY_WINDOW_TITLE
                || window_name == crate::hover_tab::HOVER_TAB_WINDOW_TITLE
                || window_name == crate::hover_tab::HOVER_TAB_LABEL
        }
    }

    /// Verdict of the startup AX canary; stage-1 resolution is gated on it so
    /// a degraded-trust process (plan §9.14) never burns per-window AX
    /// attempts that can only fail.
    static AX_CANARY_OK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    pub fn ax_canary_ok() -> bool {
        AX_CANARY_OK.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Pure sweep-cadence decision (§3 audit cadence; unit-tested): demoted
    /// mode sweeps on events (dirty) or the 2s heartbeat; poll mode sweeps at
    /// 10Hz (every other 50ms slice).
    pub(super) fn sweep_due(demoted: bool, dirty: bool, ms_since_sweep: u64) -> bool {
        if demoted {
            dirty || ms_since_sweep >= 2000
        } else {
            ms_since_sweep >= 100
        }
    }

    /// Pure tier-string chooser for the startup line — extracted so the
    /// honesty matrix (§3 ladder + §9.14) is unit-testable.
    pub(super) fn classify_tier_str(trusted: bool, symbol: bool, canary_ok: bool) -> &'static str {
        match (trusted, symbol, canary_ok) {
            (true, true, true) => "AX-subrole(T1)",
            // The §9.14 false-positive signature: preconditions look fine but
            // real window elements never arrive (inherited/degraded trust).
            (true, true, false) => "AX-degraded(->T2)",
            (false, true, _) => "AX-untrusted(->T2)",
            (_, false, _) => "AX-UNAVAILABLE(->T2)",
        }
    }

    /// Start the T2 ingest thread (idempotent). While the app is in a room —
    /// the only time any current consumer needs window state — it refreshes the
    /// snapshot at ~10Hz. Idle/out-of-room it sleeps, so it adds zero
    /// WindowServer load outside a meeting (plan §5).
    ///
    /// The §3 tier line is logged from the ingest thread AFTER the honest T1
    /// canary settles: `platform::ax::window_read_canary()` performs one real
    /// self-window read, retried briefly because our own panels lag their AX
    /// registration at startup (same birth-lag as §9.14 cause 3). A
    /// trusted+symbol check alone is a FALSE POSITIVE under inherited trust
    /// (§9.14) and must never be reported as T1.
    pub fn start(app: &tauri::AppHandle, registry: WindowRegistry) {
        use std::sync::atomic::{AtomicBool, Ordering};
        static STARTED: AtomicBool = AtomicBool::new(false);
        if STARTED.swap(true, Ordering::SeqCst) {
            return;
        }
        let app = app.clone();
        std::thread::Builder::new()
            .name("winsrv-ingest".into())
            .spawn(move || {
                // Honest T1 canary: up to ~2s of retries for our own windows'
                // AX registration to appear, then settle the verdict.
                let trusted_symbol = crate::platform::ax::ax_classification_preconditions();
                let mut canary = false;
                if trusted_symbol {
                    for _ in 0..20 {
                        if crate::platform::ax::window_read_canary() {
                            canary = true;
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
                AX_CANARY_OK.store(canary, Ordering::Relaxed);
                if canary && !crate::platform::ax::get_window_symbol_available() {
                    log::info!(
                        "winsrv: _AXUIElementGetWindow unavailable -- (pid,frame) correlation active"
                    );
                }
                log::info!(
                    "winsrv: tiers ingest=CG(T2,~10Hz) lifecycle=sweep classify=stage0+{} moves=poll",
                    classify_tier_str(
                        crate::platform::ax::process_trusted(),
                        crate::platform::ax::ax_mechanism_available(),
                        canary
                    )
                );
                // T1 lifecycle feed (#747): the observer hub is ADDITIVE in
                // this phase — the sweep keeps its cadence (demotion needs the
                // §4 gesture path first; AX `moved` fires at drag END and a
                // demoted sweep would freeze border/telepointer during drags).
                if canary {
                    crate::platform::ax_observer::start();
                }
                crate::platform::gesture_tap::start(&app);
                // T0 event stream (#748): registration is grant-independent;
                // DELIVERY needs Screen Recording (§9.5). Health is judged by
                // sls_events_live() (first real event), never registration.
                crate::platform::sls::start_event_stream();
                let mut sls_reported = false;
                let mut sls_moves_reported = false;
                let mut nudge_fired = false;
                let mut observers_reported = false;
                let mut demotion_reported = false;
                let mut last_sweep = std::time::Instant::now();
                let mut was_in_room = false;
                loop {
                    let in_room = tauri::Manager::try_state::<crate::session::SessionState>(&app)
                        .map(|s| s.current_room_name().is_some())
                        .unwrap_or(false);
                    if in_room != was_in_room {
                        was_in_room = in_room;
                        crate::platform::gesture_tap::set_enabled(in_room);
                    }
                    if !in_room {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        continue;
                    }
                    // Sweep demotion (§3 audit cadence): with BOTH event feeds
                    // live (observers for lifecycle/state, gesture tap for
                    // drags) the full sweep drops to dirty-triggered (≤50ms
                    // after an event) + 2s heartbeat. Any feed missing -> 10Hz
                    // poll exactly as before.
                    // Demotion v2 (#748): drag coverage can come from EITHER
                    // the gesture tap OR proven SLS move delivery (806 fires
                    // per step for real drags, §9.1); observers stay required
                    // for subrole rechecks; canary for classification trust.
                    let demoted = canary
                        && crate::platform::ax_observer::observers_live()
                        && (crate::platform::gesture_tap::gesture_live()
                            || crate::platform::sls::sls_moves_live());
                    let dirty = crate::platform::ax_observer::take_dirty();
                    let due = sweep_due(
                        demoted,
                        dirty,
                        last_sweep.elapsed().as_millis() as u64,
                    );
                    if due {
                        refresh_once(&registry);
                        last_sweep = std::time::Instant::now();
                        // Re-converge the per-window SLS subscription toward
                        // the snapshot (yabai's full-list pattern; the sls
                        // thread dedupes, so unchanged sets are free).
                        {
                            let snap = registry.snapshot();
                            crate::platform::sls::subscribe_windows(snap.order.clone());
                        }
                        // ACTIVE move canary (§3): after the FIRST subscription
                        // push (806 is subscription-gated, §9.1 -- a fixed-delay
                        // nudge fired before convergence and proved nothing),
                        // nudge the offscreen hover-tab panel 1pt on the main
                        // thread (crash class 1). If 806 delivery works at all
                        // it fires for this move, making sls_moves_live() a
                        // proven signal even on an idle desktop -- without
                        // creating/destroying any panel (crash class 2).
                        if !nudge_fired {
                            nudge_fired = true;
                            let app2 = app.clone();
                            std::thread::spawn(move || {
                                // let the sls thread drain the subscription
                                std::thread::sleep(std::time::Duration::from_millis(700));
                                let app3 = app2.clone();
                                let _ = app2.run_on_main_thread(move || {
                                    // First OFFSCREEN-PARKED panel wins: a
                                    // visible panel must never be nudged (the
                                    // hover pill can be live-tracking; a 1px
                                    // wiggle there is a user-visible glitch).
                                    for label in ["share-notice", "menubar-popover", "hover-tab"] {
                                        let Some(w) =
                                            tauri::Manager::get_webview_window(&app3, label)
                                        else {
                                            continue;
                                        };
                                        let Ok(pos) = w.outer_position() else { continue };
                                        if pos.x < -5000 {
                                            let _ = w.set_position(
                                                tauri::PhysicalPosition::new(pos.x + 1, pos.y),
                                            );
                                            let _ = w.set_position(pos);
                                            log::info!(
                                                "winsrv: move canary nudged '{label}' at ({}, {})",
                                                pos.x,
                                                pos.y
                                            );
                                            return;
                                        }
                                    }
                                    log::info!(
                                        "winsrv: move canary skipped -- no offscreen-parked panel (moves proven by first real drag instead)"
                                    );
                                });
                            });
                        }
                        if !sls_reported && crate::platform::sls::sls_events_live() {
                            sls_reported = true;
                            log::info!(
                                "winsrv: T0 upgraded -- SLS event stream live (moves={} lifecycle={})",
                                crate::platform::sls::sls_moves_live(),
                                crate::platform::sls::sls_lifecycle_live()
                            );
                        }
                        // Per-capability upgrades log on their own flip: the
                        // single T0 line froze moves=false when a lifecycle
                        // event beat the nudge canary by milliseconds -- the
                        // B2 battery near-miss.
                        if !sls_moves_reported && crate::platform::sls::sls_moves_live() {
                            sls_moves_reported = true;
                            log::info!("winsrv: T0 moves capability live (806 delivery proven)");
                        }
                        if canary {
                            sync_observers(&registry);
                            if !observers_reported
                                && crate::platform::ax_observer::observers_live()
                            {
                                observers_reported = true;
                                log::info!(
                                    "winsrv: lifecycle tier upgraded -- AX-observers active (sweep remains audit)"
                                );
                            }
                        }
                        resolve_pending_stage1(&registry);
                    }
                    if demoted && !demotion_reported {
                        demotion_reported = true;
                        log::info!(
                            "winsrv: sweep demoted -- event-triggered + 2s heartbeat (observers + gesture tap live)"
                        );
                    } else if !demoted && demotion_reported {
                        demotion_reported = false;
                        log::info!("winsrv: sweep restored to 10Hz poll (an event feed went down)");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            })
            .ok();
    }

    /// One CG sweep -> registry snapshot. Needs window NAMES (to classify own
    /// chrome), so it uses the name-bearing enumeration.
    pub fn refresh_once(registry: &WindowRegistry) {
        let Some(entries) = crate::platform::cg::onscreen_windows() else {
            return;
        };
        let self_pid = std::process::id() as i32;
        let rows: Vec<(u32, f64, f64, f64, f64, i64, f64, i32, String)> = entries
            .into_iter()
            .filter_map(|e| {
                let wid = u32::try_from(e.number).ok()?;
                Some((
                    wid,
                    e.x,
                    e.y,
                    e.w,
                    e.h,
                    e.layer,
                    e.alpha,
                    i32::try_from(e.owner_pid).unwrap_or(-1),
                    e.name,
                ))
            })
            .collect();
        registry.ingest_rows(&rows, self_pid, &NameChromeOracle);
    }

    /// Stage-1 (§3): resolve AX subroles for ONE app's pending windows per
    /// tick, in a single batched AX pass (one `kAXWindows` copy classifies
    /// every pending window of that app — #747 AX-cost audit; previously each
    /// window re-copied the array).
    ///
    /// Runs ONLY from the 10Hz ingest thread loop — deliberately NOT part of
    /// `refresh_once`, which `refresh_now()` exposes to hover's 60Hz follow
    /// path and remote-control: one busy app's main thread can block an AX
    /// call up to 250ms, which must never land on a latency-critical path
    /// (the user's prior system hit exactly this; plan §3 stage-1 rules).
    ///
    /// Gated on the honest canary (§9.14): under degraded trust every attempt
    /// can only fail. Apps that fail at the APP level (no AX server, timeout)
    /// go `AXDead` after [`AX_APP_MAX_FAILURES`] strikes and cost nothing
    /// further; their windows keep their stage-0 class.
    pub fn resolve_pending_stage1(registry: &WindowRegistry) {
        if !ax_canary_ok() {
            return;
        }
        // Subrole RECHECK (§3 mutation caveat) runs FIRST — the pending-window
        // scan below early-returns when everything is settled, which is
        // precisely when a state-event recheck is most likely queued. Placing
        // the drain after that return starved it: the G3 morph gate failed
        // (stale Popup survived chrome gain) with the AXWindowResized event
        // demonstrably delivered. Old value kept until the new one lands.
        let recheck = crate::platform::ax_observer::drain_recheck();
        if !recheck.is_empty() {
            let snap = registry.snapshot();
            let mut by_pid: std::collections::HashMap<i32, Vec<(u32, (f64, f64, f64, f64))>> =
                std::collections::HashMap::new();
            for wid in recheck {
                if let Some(r) = snap.by_id.get(&wid) {
                    if r.class == WindowClass::Unknown {
                        by_pid
                            .entry(r.owner_pid)
                            .or_default()
                            .push((r.wid, (r.rx, r.ry, r.rw, r.rh)));
                    }
                }
            }
            drop(snap);
            for (pid, wanted) in by_pid {
                registry.recheck_kinds_for_app(pid, &wanted, |pid, wanted| {
                    map_platform_outcome(crate::platform::ax::resolve_window_kinds_for_app(
                        pid, wanted,
                    ))
                });
            }
        }
        let snap = registry.snapshot();
        // Frontmost unsettled foreign window picks the app; batch every
        // unsettled window of that app into one pass.
        let Some(pid) = snap
            .records_front_to_back()
            .find(|r| {
                r.class == WindowClass::Unknown
                    && !registry.kind_settled(r.wid)
                    && !registry.ax_pid_dead(r.owner_pid)
            })
            .map(|r| r.owner_pid)
        else {
            return;
        };
        let wanted: Vec<(u32, (f64, f64, f64, f64))> = snap
            .records_front_to_back()
            .filter(|r| {
                r.owner_pid == pid
                    && r.class == WindowClass::Unknown
                    && !registry.kind_settled(r.wid)
            })
            .map(|r| (r.wid, (r.rx, r.ry, r.rw, r.rh)))
            .collect();
        drop(snap);
        registry.ensure_kinds_resolved_for_app(pid, &wanted, |pid, wanted| {
            map_platform_outcome(crate::platform::ax::resolve_window_kinds_for_app(
                pid, wanted,
            ))
        });
        // Log non-Standard classifications (diagnostic, like the tier line):
        // confirms stage-1 ran and what it decided -- the oracle the
        // popup-filter live probe reads.
        for (wid, _) in wanted {
            if let Some(kind) = registry.window_kind(wid) {
                if kind != super::AxKind::Standard {
                    log::info!("winsrv: classified window {wid} as {kind:?}");
                }
            }
        }
    }

    /// Converge the observer set toward the apps owning foreign layer-0 real
    /// windows (lazy registration, §3).
    fn sync_observers(registry: &WindowRegistry) {
        let snap = registry.snapshot();
        let pids: Vec<i32> = {
            let mut set = std::collections::HashSet::new();
            for r in snap.records_front_to_back() {
                if r.class == WindowClass::Unknown && r.is_real {
                    set.insert(r.owner_pid);
                }
            }
            set.into_iter().collect()
        };
        crate::platform::ax_observer::sync_apps(pids);
    }

    /// Platform outcome -> registry-neutral result (shared by resolve + recheck).
    fn map_platform_outcome(o: crate::platform::ax::AppKindsOutcome) -> super::AppKindsResult {
        match o {
            crate::platform::ax::AppKindsOutcome::Resolved(kinds) => {
                super::AppKindsResult::Resolved(
                    kinds
                        .into_iter()
                        .map(|(wid, k)| {
                            (
                                wid,
                                match k {
                                    crate::platform::ax::AxKind::Standard => {
                                        super::AxKind::Standard
                                    }
                                    crate::platform::ax::AxKind::Dialog => super::AxKind::Dialog,
                                    crate::platform::ax::AxKind::Popup => super::AxKind::Popup,
                                },
                            )
                        })
                        .collect(),
                )
            }
            crate::platform::ax::AppKindsOutcome::AppUnavailable => {
                super::AppKindsResult::AppUnavailable
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysForeign;
    impl OwnChromeOracle for AlwaysForeign {
        fn is_decorative(&self, _: &str) -> bool {
            false
        }
    }

    fn frame(x: i32, y: i32, w: i32, h: i32) -> WindowFrame {
        WindowFrame {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn ingest_never_drops_windows_and_preserves_order() {
        let reg = WindowRegistry::new();
        let rows = vec![
            (10, 0.0, 0.0, 800.0, 600.0, 0, 1.0, 111, String::new()),
            (20, 0.0, 0.0, 2560.0, 1440.0, 25, 1.0, 222, "Dock".into()), // system chrome
            (30, 0.0, 0.0, 10.0, 10.0, 0, 1.0, 333, String::new()),      // sub-40, still kept
        ];
        reg.ingest_rows(&rows, 999, &AlwaysForeign);
        let s = reg.snapshot();
        assert_eq!(
            s.order,
            vec![10, 20, 30],
            "every window kept, order preserved"
        );
        assert_eq!(s.by_id.len(), 3);
    }

    #[test]
    fn registered_region_identity_wins_over_stale_cg_owner() {
        assert_eq!(
            super::classify_stage0(3, 7, 42, false, true),
            WindowClass::RegionSelector,
            "a registered Petal View remains a selector when the VM reports a stale owner/layer"
        );
        assert_eq!(
            super::classify_stage0(0, 42, 42, false, false),
            WindowClass::PetalOwned { decorative: false },
            "ordinary Petal windows still use the normal owner classification"
        );
    }

    #[test]
    fn stage0_classes_and_is_real() {
        let reg = WindowRegistry::new();
        let rows = vec![
            (10, 0.0, 0.0, 800.0, 600.0, 0, 1.0, 111, String::new()), // foreign real
            (20, 0.0, 0.0, 100.0, 100.0, 25, 1.0, 222, String::new()), // system chrome
            (30, 0.0, 0.0, 60.0, 60.0, 0, 1.0, 999, "Share Border".into()), // own decorative
            (40, 0.0, 0.0, 400.0, 300.0, 0, 1.0, 999, "Main".into()), // own content
            (50, 0.0, 0.0, 10.0, 10.0, 0, 1.0, 111, String::new()),   // foreign, sub-40
        ];
        struct Chrome;
        impl OwnChromeOracle for Chrome {
            fn is_decorative(&self, n: &str) -> bool {
                n == "Share Border"
            }
        }
        reg.ingest_rows(&rows, 999, &Chrome);
        let s = reg.snapshot();
        assert_eq!(s.by_id[&10].class, WindowClass::Unknown);
        assert!(s.by_id[&10].is_real);
        assert_eq!(s.by_id[&20].class, WindowClass::SystemChrome);
        assert_eq!(
            s.by_id[&30].class,
            WindowClass::PetalOwned { decorative: true }
        );
        assert_eq!(
            s.by_id[&40].class,
            WindowClass::PetalOwned { decorative: false }
        );
        assert!(!s.by_id[&50].is_real, "sub-40pt is not is_real");
    }

    #[test]
    fn order_gen_bumps_only_on_order_change() {
        let reg = WindowRegistry::new();
        let a = vec![(10, 0.0, 0.0, 80.0, 80.0, 0, 1.0, 1, String::new())];
        reg.ingest_rows(&a, 999, &AlwaysForeign);
        let g0 = reg.order_generation();
        // same order, moved frame -> order_gen unchanged
        let b = vec![(10, 5.0, 5.0, 80.0, 80.0, 0, 1.0, 1, String::new())];
        reg.ingest_rows(&b, 999, &AlwaysForeign);
        assert_eq!(
            reg.order_generation(),
            g0,
            "frame move does not bump order_gen"
        );
        // different order -> bump
        let c = vec![
            (20, 0.0, 0.0, 80.0, 80.0, 0, 1.0, 1, String::new()),
            (10, 5.0, 5.0, 80.0, 80.0, 0, 1.0, 1, String::new()),
        ];
        reg.ingest_rows(&c, 999, &AlwaysForeign);
        assert_eq!(
            reg.order_generation(),
            g0 + 1,
            "order change bumps order_gen"
        );
    }

    #[test]
    fn occlusion_matches_the_hand_computed_fraction() {
        let reg = WindowRegistry::new();
        // target 100x100 at (0,0); a foreign opaque window covers its right half.
        reg.ingest_rows(
            &[
                (5, 50.0, 0.0, 50.0, 100.0, 0, 1.0, 111, String::new()), // front, covers right half
                (10, 0.0, 0.0, 100.0, 100.0, 0, 1.0, 222, String::new()), // target
            ],
            999,
            &AlwaysForeign,
        );
        // 50x100 covered of 100x100 = 0.5
        assert_eq!(reg.occlusion(10, 999), Some(0.5));
        // an OWN window in front does not count as occlusion
        let reg2 = WindowRegistry::new();
        reg2.ingest_rows(
            &[
                (5, 0.0, 0.0, 100.0, 100.0, 0, 1.0, 999, String::new()), // OUR window on top
                (10, 0.0, 0.0, 100.0, 100.0, 0, 1.0, 222, String::new()),
            ],
            999,
            &AlwaysForeign,
        );
        assert_eq!(
            reg2.occlusion(10, 999),
            Some(0.0),
            "own windows never occlude"
        );
        // a transparent front window does not count
        let reg3 = WindowRegistry::new();
        reg3.ingest_rows(
            &[
                (5, 0.0, 0.0, 100.0, 100.0, 0, 0.5, 111, String::new()), // alpha 0.5
                (10, 0.0, 0.0, 100.0, 100.0, 0, 1.0, 222, String::new()),
            ],
            999,
            &AlwaysForeign,
        );
        assert_eq!(
            reg3.occlusion(10, 999),
            Some(0.0),
            "transparent windows never occlude"
        );
    }

    /// AX-BUDGET counting oracle (§3 / §7.4): ≤1 WINNING resolver pass per
    /// window lifetime, `window_kind` (the consumer read) never triggers it,
    /// batching classifies MANY windows of one app in ONE pass, and the cache
    /// is pruned when a window is destroyed (so a reappearing id re-resolves
    /// — one more pass, not unbounded).
    #[test]
    fn ax_kind_resolves_at_most_once_per_lifetime_and_prunes_on_destroy() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reg = WindowRegistry::new();
        let calls = AtomicUsize::new(0);
        let resolver = |_pid: i32, wanted: &HashMap<u32, (f64, f64, f64, f64)>| {
            calls.fetch_add(1, Ordering::SeqCst);
            AppKindsResult::Resolved(wanted.keys().map(|&w| (w, AxKind::Dialog)).collect())
        };
        // two windows of the same app present
        reg.ingest_rows(
            &[
                (10, 0.0, 0.0, 80.0, 80.0, 0, 1.0, 111, String::new()),
                (11, 0.0, 0.0, 80.0, 80.0, 0, 1.0, 111, String::new()),
            ],
            999,
            &AlwaysForeign,
        );
        // consumer reads never resolve
        assert_eq!(reg.window_kind(10), None);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "window_kind must not trigger AX"
        );
        // ONE batched pass settles BOTH windows
        reg.ensure_kinds_resolved_for_app(
            111,
            &[(10, (0.0, 0.0, 80.0, 80.0)), (11, (0.0, 0.0, 90.0, 90.0))],
            resolver,
        );
        assert_eq!(reg.window_kind(10), Some(AxKind::Dialog));
        assert_eq!(reg.window_kind(11), Some(AxKind::Dialog));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "one pass for the whole app"
        );
        // repeated calls are no-ops (settled wids filtered before the resolver)
        reg.ensure_kinds_resolved_for_app(
            111,
            &[(10, (0.0, 0.0, 80.0, 80.0)), (11, (0.0, 0.0, 90.0, 90.0))],
            resolver,
        );
        reg.ensure_kinds_resolved_for_app(111, &[(10, (0.0, 0.0, 80.0, 80.0))], resolver);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "at most one winning pass");
        // windows destroyed (absent from next snapshot) -> cache pruned
        reg.ingest_rows(
            &[(20, 0.0, 0.0, 80.0, 80.0, 0, 1.0, 222, String::new())],
            999,
            &AlwaysForeign,
        );
        assert_eq!(
            reg.window_kind(10),
            None,
            "kind pruned when window destroyed"
        );
        // a reappearing id re-resolves in exactly one more pass
        reg.ingest_rows(
            &[(10, 0.0, 0.0, 80.0, 80.0, 0, 1.0, 111, String::new())],
            999,
            &AlwaysForeign,
        );
        reg.ensure_kinds_resolved_for_app(111, &[(10, (0.0, 0.0, 80.0, 80.0))], resolver);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "reappeared window: one more pass"
        );
    }

    /// §3 `AXDead` (#747 audit): an app whose AX server fails at the APP
    /// level goes dead after AX_APP_MAX_FAILURES strikes — no further
    /// resolver calls for ANY of its windows (each failed call can block up
    /// to 250ms; never pay that per window). Cleared when the pid leaves the
    /// snapshot (app quit), so a relaunched app gets a fresh chance.
    #[test]
    fn app_level_ax_failures_mark_pid_dead_and_stop_all_attempts() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reg = WindowRegistry::new();
        reg.ingest_rows(
            &[
                (10, 0.0, 0.0, 80.0, 80.0, 0, 1.0, 111, String::new()),
                (11, 0.0, 0.0, 80.0, 80.0, 0, 1.0, 111, String::new()),
            ],
            999,
            &AlwaysForeign,
        );
        let calls = AtomicUsize::new(0);
        let failing = |_pid: i32, _wanted: &HashMap<u32, (f64, f64, f64, f64)>| {
            calls.fetch_add(1, Ordering::SeqCst);
            AppKindsResult::AppUnavailable
        };
        for _ in 0..(AX_APP_MAX_FAILURES as usize + 4) {
            reg.ensure_kinds_resolved_for_app(
                111,
                &[(10, (0.0, 0.0, 80.0, 80.0)), (11, (0.0, 0.0, 90.0, 90.0))],
                failing,
            );
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            AX_APP_MAX_FAILURES as usize,
            "dead pid stops costing resolver calls"
        );
        assert!(reg.ax_pid_dead(111));
        assert_eq!(reg.window_kind(10), None, "windows keep stage-0 class");
        // app quits (pid leaves snapshot) -> dead marker cleared
        reg.ingest_rows(
            &[(30, 0.0, 0.0, 80.0, 80.0, 0, 1.0, 222, String::new())],
            999,
            &AlwaysForeign,
        );
        assert!(!reg.ax_pid_dead(111), "AXDead cleared on app exit");
        // one app-level success resets strikes for a live pid
        let reg2 = WindowRegistry::new();
        reg2.ingest_rows(
            &[(10, 0.0, 0.0, 80.0, 80.0, 0, 1.0, 111, String::new())],
            999,
            &AlwaysForeign,
        );
        reg2.ensure_kinds_resolved_for_app(111, &[(10, (0.0, 0.0, 80.0, 80.0))], |_p, _w| {
            AppKindsResult::AppUnavailable
        });
        reg2.ensure_kinds_resolved_for_app(111, &[(10, (0.0, 0.0, 80.0, 80.0))], |_p, _w| {
            AppKindsResult::AppUnavailable
        });
        reg2.ensure_kinds_resolved_for_app(111, &[(10, (0.0, 0.0, 80.0, 80.0))], |_p, _w| {
            AppKindsResult::Resolved(HashMap::new()) // readable array, window absent
        });
        assert!(
            !reg2.ax_pid_dead(111),
            "an app-level success resets strikes"
        );
    }

    /// DEGRADATION DRILL (§7.4 DoD, offline half): when AX is unavailable the
    /// resolver returns `None`; after the bounded retry budget the window
    /// settles as `None` — the T1 tier degrades cleanly to stage-0-only (T2)
    /// with no consumer-visible change (hover would filter nothing) and no
    /// unbounded AX churn.
    #[test]
    fn ax_unavailable_degrades_to_stage0_only() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reg = WindowRegistry::new();
        reg.ingest_rows(
            &[(10, 0.0, 0.0, 800.0, 600.0, 0, 1.0, 111, String::new())],
            999,
            &AlwaysForeign,
        );
        // AX "unavailable": every app-level pass fails -> pid AXDead after
        // AX_APP_MAX_FAILURES strikes, zero further resolver calls.
        let calls = AtomicUsize::new(0);
        for _ in 0..(AX_APP_MAX_FAILURES as usize + 5) {
            reg.ensure_kinds_resolved_for_app(
                111,
                &[(10, (0.0, 0.0, 80.0, 80.0))],
                |_pid, _wanted| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    AppKindsResult::AppUnavailable
                },
            );
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            AX_APP_MAX_FAILURES as usize,
            "app-level failures bounded by AXDead, not per-window budgets"
        );
        assert_eq!(
            reg.window_kind(10),
            None,
            "no kind when AX is unavailable -> hover filters nothing (T2 behavior)"
        );
        assert!(reg.ax_pid_dead(111));
    }

    /// #747 retry fix: a window whose AX registration LAGS its CG appearance
    /// (every window born in-room) must not be permanently mis-cached by the
    /// first `None` — a later retry that finds the window wins and settles.
    #[test]
    fn ax_kind_retries_transient_none_then_settles_on_success() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reg = WindowRegistry::new();
        reg.ingest_rows(
            &[(10, 0.0, 0.0, 300.0, 200.0, 0, 1.0, 111, String::new())],
            999,
            &AlwaysForeign,
        );
        let calls = AtomicUsize::new(0);
        // First two passes: array readable, window not in it yet (birth lag).
        for _ in 0..2 {
            reg.ensure_kinds_resolved_for_app(
                111,
                &[(10, (0.0, 0.0, 80.0, 80.0))],
                |_pid, _wanted| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    AppKindsResult::Resolved(HashMap::new())
                },
            );
        }
        assert!(
            !reg.kind_settled(10),
            "still retryable after transient misses"
        );
        assert_eq!(reg.window_kind(10), None, "unsettled reads as no-kind");
        assert!(!reg.ax_pid_dead(111), "misses are not app-level strikes");
        // Third pass: AX has caught up.
        reg.ensure_kinds_resolved_for_app(111, &[(10, (0.0, 0.0, 80.0, 80.0))], |_pid, _wanted| {
            calls.fetch_add(1, Ordering::SeqCst);
            AppKindsResult::Resolved([(10, AxKind::Popup)].into_iter().collect())
        });
        assert_eq!(reg.window_kind(10), Some(AxKind::Popup));
        assert!(reg.kind_settled(10));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        // Settled: no further AX calls ever.
        reg.ensure_kinds_resolved_for_app(111, &[(10, (0.0, 0.0, 80.0, 80.0))], |_pid, _wanted| {
            panic!("must not re-resolve after a successful resolution")
        });
    }

    /// #747 audit: a window that never appears in its app's READABLE AX array
    /// (e.g. a window kind AX will never serve) settles as permanent `None`
    /// after AX_KIND_MAX_ATTEMPTS misses — bounded churn, then silence.
    #[test]
    fn persistent_miss_settles_after_the_bounded_look_budget() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reg = WindowRegistry::new();
        reg.ingest_rows(
            &[(10, 0.0, 0.0, 80.0, 80.0, 0, 1.0, 111, String::new())],
            999,
            &AlwaysForeign,
        );
        let calls = AtomicUsize::new(0);
        for _ in 0..(AX_KIND_MAX_ATTEMPTS as usize + 5) {
            reg.ensure_kinds_resolved_for_app(
                111,
                &[(10, (0.0, 0.0, 80.0, 80.0))],
                |_pid, _wanted| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    AppKindsResult::Resolved(HashMap::new()) // readable, absent
                },
            );
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            AX_KIND_MAX_ATTEMPTS as usize,
            "misses bounded by AX_KIND_MAX_ATTEMPTS, then settled"
        );
        assert!(reg.kind_settled(10));
        assert_eq!(reg.window_kind(10), None);
        assert!(!reg.ax_pid_dead(111), "misses never kill the pid");
    }

    /// §3 subrole-mutation recheck (#747): a state event re-resolves a SETTLED
    /// kind, replacing it only on success — never unsettling (no consumer
    /// flicker), never counting misses, never striking the pid.
    #[test]
    fn state_event_recheck_replaces_on_success_and_keeps_old_on_failure() {
        let reg = WindowRegistry::new();
        reg.ingest_rows(
            &[(10, 0.0, 0.0, 300.0, 200.0, 0, 1.0, 111, String::new())],
            999,
            &AlwaysForeign,
        );
        // Settle as Popup first.
        reg.ensure_kinds_resolved_for_app(111, &[(10, (0.0, 0.0, 300.0, 200.0))], |_p, _w| {
            AppKindsResult::Resolved([(10, AxKind::Popup)].into_iter().collect())
        });
        assert_eq!(reg.window_kind(10), Some(AxKind::Popup));
        // Recheck miss (window absent from readable array): old value KEPT.
        reg.recheck_kinds_for_app(111, &[(10, (0.0, 0.0, 300.0, 200.0))], |_p, _w| {
            AppKindsResult::Resolved(HashMap::new())
        });
        assert_eq!(
            reg.window_kind(10),
            Some(AxKind::Popup),
            "miss keeps old kind"
        );
        assert!(reg.kind_settled(10), "recheck never unsettles");
        // Recheck app-level failure: old value kept, pid NOT struck.
        reg.recheck_kinds_for_app(111, &[(10, (0.0, 0.0, 300.0, 200.0))], |_p, _w| {
            AppKindsResult::AppUnavailable
        });
        assert_eq!(reg.window_kind(10), Some(AxKind::Popup));
        assert!(
            !reg.ax_pid_dead(111),
            "recheck failures never strike the pid"
        );
        // Recheck success: value replaced (the window gained chrome).
        reg.recheck_kinds_for_app(111, &[(10, (0.0, 0.0, 300.0, 200.0))], |_p, _w| {
            AppKindsResult::Resolved([(10, AxKind::Standard)].into_iter().collect())
        });
        assert_eq!(
            reg.window_kind(10),
            Some(AxKind::Standard),
            "success replaces"
        );
    }

    /// §3 audit cadence (#747): demoted mode sweeps on events or the 2s
    /// heartbeat; any missing feed keeps the 10Hz poll.
    /// macOS-only: the sweep-due logic lives in the macOS-gated `ingest`
    /// module; Windows has no registry ingest to schedule.
    #[cfg(target_os = "macos")]
    #[test]
    fn sweep_cadence_demotes_only_with_both_feeds() {
        use super::ingest::sweep_due;
        // poll mode: 10Hz regardless of dirty
        assert!(!sweep_due(false, false, 50));
        assert!(sweep_due(false, false, 100));
        assert!(sweep_due(false, true, 100));
        // demoted: event-triggered ...
        assert!(sweep_due(true, true, 10));
        assert!(!sweep_due(true, false, 1999));
        // ... plus heartbeat
        assert!(sweep_due(true, false, 2000));
    }

    /// #747 §4 gesture support: per-window frame update publishes new raw +
    /// truncated frames, bumps `gen`, and leaves order/order_gen untouched.
    #[test]
    fn update_window_frame_bumps_gen_and_keeps_order() {
        let reg = WindowRegistry::new();
        reg.ingest_rows(
            &[
                (10, 0.0, 0.0, 300.0, 200.0, 0, 1.0, 111, String::new()),
                (11, 50.0, 50.0, 300.0, 200.0, 0, 1.0, 222, String::new()),
            ],
            999,
            &AlwaysForeign,
        );
        let before = reg.snapshot();
        reg.update_window_frame(10, 120.5, 130.4, 300.0, 200.0);
        let after = reg.snapshot();
        assert_eq!(after.by_id[&10].rx, 120.5);
        assert_eq!(after.by_id[&10].frame.x, 121, "rounded frame follows raw");
        assert_eq!(after.order, before.order, "drag never reorders mid-gesture");
        assert_eq!(after.order_gen, before.order_gen);
        assert!(
            after.gen > before.gen,
            "gen bumps so readers see the change"
        );
        // unknown wid: no-op, no publish
        let g = reg.snapshot().gen;
        reg.update_window_frame(999, 0.0, 0.0, 1.0, 1.0);
        assert_eq!(reg.snapshot().gen, g);
    }

    /// #747 §4 gesture targeting: topmost FOREIGN layer-0 window at a point;
    /// own windows and non-zero layers are transparent to it.
    #[test]
    fn topmost_foreign_at_skips_own_and_chrome() {
        let reg = WindowRegistry::new();
        let self_pid = 999;
        reg.ingest_rows(
            &[
                // frontmost: our own window at the point -> skipped
                (5, 0.0, 0.0, 400.0, 400.0, 0, 1.0, self_pid, String::new()),
                // next: chrome layer at the point -> skipped
                (6, 0.0, 0.0, 400.0, 400.0, 25, 1.0, 111, String::new()),
                // next: foreign layer-0 -> the target
                (7, 0.0, 0.0, 400.0, 400.0, 0, 1.0, 111, String::new()),
                (8, 0.0, 0.0, 400.0, 400.0, 0, 1.0, 222, String::new()),
            ],
            self_pid,
            &AlwaysForeign,
        );
        assert_eq!(reg.topmost_foreign_at(100.0, 100.0, self_pid), Some(7));
        assert_eq!(
            reg.topmost_foreign_at(4000.0, 100.0, self_pid),
            None,
            "miss outside"
        );
    }

    /// §3 ladder honesty matrix (#747 §9.14): the tier line must never claim
    /// T1 from trusted+symbol alone — inherited trust makes that a false
    /// positive; only a passed window-read canary earns "AX-subrole(T1)".
    /// macOS-only: the tier classifier lives in the macOS-gated `ingest`
    /// module.
    #[cfg(target_os = "macos")]
    #[test]
    fn tier_line_never_claims_t1_without_the_canary() {
        use super::ingest::classify_tier_str;
        assert_eq!(classify_tier_str(true, true, true), "AX-subrole(T1)");
        // The §9.14 signature: preconditions fine, real reads degraded.
        assert_eq!(classify_tier_str(true, true, false), "AX-degraded(->T2)");
        assert_eq!(classify_tier_str(false, true, false), "AX-untrusted(->T2)");
        assert_eq!(
            classify_tier_str(true, false, false),
            "AX-UNAVAILABLE(->T2)"
        );
        assert_eq!(
            classify_tier_str(false, false, false),
            "AX-UNAVAILABLE(->T2)"
        );
    }

    #[test]
    fn read_api_frame_exists_owner() {
        let reg = WindowRegistry::new();
        reg.ingest_rows(
            &[(10, 1.0, 2.0, 30.0, 40.0, 0, 1.0, 77, String::new())],
            999,
            &AlwaysForeign,
        );
        assert_eq!(reg.frame(10), Some(frame(1, 2, 30, 40)));
        assert!(reg.exists(10));
        assert!(!reg.exists(11));
        assert_eq!(reg.owner_pid(10), Some(77));
        assert_eq!(reg.owner_pid(11), None);
    }
}
