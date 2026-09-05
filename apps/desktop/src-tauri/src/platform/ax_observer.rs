//! Per-app AX observers (#747, Phase 3 T1 lifecycle feed).
//!
//! One dedicated "winsrv-ax" thread owns a `CFRunLoop` hosting every
//! `AXObserver`'s run-loop source (the `takeover.rs` precedent). The ingest
//! thread drives registration by DIFFING the set of foreign pids that own
//! layer-0 real windows (lazy registration, §3: menu-bar agents and
//! background apps never get an observer); the observer callback does minimal
//! work — set a dirty flag, and for window-carrying state events enqueue the
//! wid for a SUBROLE RECHECK (§3: AXSubrole mutates with window state —
//! Standard ↔ Dialog ↔ Floating — so a settled kind is re-resolved on its
//! window's AX events, keeping the old value until the new one lands so
//! consumers never see a flicker of "unclassified").
//!
//! This tier is ADDITIVE in this commit: the T2 sweep keeps its in-room
//! cadence. Demoting the sweep to debounced-on-event + heartbeat requires the
//! §4 gesture fast path first (AX `moved` fires at drag END — a demoted sweep
//! would freeze border/telepointer follow during drags).

#![cfg(target_os = "macos")]

use std::collections::{HashMap, HashSet};
use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

// ---- FFI ------------------------------------------------------------------
// ApplicationServices AX functions link via remote_control's existing extern
// block (same crate); CFRunLoop comes from CoreFoundation linked by cg.rs.
extern "C" {
    fn AXObserverCreate(
        pid: i32,
        callback: extern "C" fn(*const c_void, *const c_void, *const c_void, *mut c_void),
        out: *mut *const c_void,
    ) -> i32;
    fn AXObserverAddNotification(
        observer: *const c_void,
        element: *const c_void,
        notification: *const c_void,
        refcon: *mut c_void,
    ) -> i32;
    fn AXObserverGetRunLoopSource(observer: *const c_void) -> *const c_void;
    fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
    fn CFRunLoopGetCurrent() -> *const c_void;
    fn CFRunLoopAddSource(rl: *const c_void, source: *const c_void, mode: *const c_void);
    fn CFRunLoopRemoveSource(rl: *const c_void, source: *const c_void, mode: *const c_void);
    fn CFRunLoopRunInMode(
        mode: *const c_void,
        seconds: f64,
        return_after_source_handled: bool,
    ) -> i32;
    fn CFRunLoopWakeUp(rl: *const c_void);
    fn CFRelease(cf: *const c_void);
    static kCFRunLoopDefaultMode: *const c_void;
}

/// The notifications registered per app. Created/destroyed are the lifecycle
/// feed (Sequoia's destroyed-hole is covered by the audit sweep, §2);
/// moved/resized/miniaturized are the SUBROLE-RECHECK triggers (§3 mutation
/// caveat) — a moved event also implies fresh geometry, but geometry comes
/// from the sweep, not from AX.
const NOTIFICATIONS: [&std::ffi::CStr; 6] = [
    c"AXWindowCreated",
    c"AXUIElementDestroyed",
    c"AXWindowMoved",
    c"AXWindowResized",
    c"AXWindowMiniaturized",
    c"AXWindowDeminiaturized",
];

/// What one AX notification means for the registry. Pure; unit-tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserverAction {
    /// Window set likely changed -> mark snapshot dirty (sweep reconciles).
    Lifecycle,
    /// Window state changed -> dirty + recheck the window's cached subrole.
    StateRecheck,
    /// Not one of ours (defensive; AX can deliver others).
    Ignore,
}

pub fn classify_notification(name: &str) -> ObserverAction {
    match name {
        "AXWindowCreated" | "AXUIElementDestroyed" => ObserverAction::Lifecycle,
        "AXWindowMoved" | "AXWindowResized" | "AXWindowMiniaturized"
        | "AXWindowDeminiaturized" => ObserverAction::StateRecheck,
        _ => ObserverAction::Ignore,
    }
}

/// Which apps should have an observer, given the snapshot's foreign layer-0
/// real-window owners and the currently-registered set. Pure; unit-tested.
/// Returns (to_register, to_unregister).
pub fn diff_observer_apps(
    should_have: &HashSet<i32>,
    registered: &HashSet<i32>,
) -> (Vec<i32>, Vec<i32>) {
    let mut add: Vec<i32> = should_have.difference(registered).copied().collect();
    let mut remove: Vec<i32> = registered.difference(should_have).copied().collect();
    add.sort_unstable();
    remove.sort_unstable();
    (add, remove)
}

// ---- Shared event state (observer thread -> ingest thread) ----------------

static DIRTY: AtomicBool = AtomicBool::new(false);
static RECHECK: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
/// True once at least one app observer registered successfully this session;
/// feeds the tier line (lifecycle=AX-observers vs sweep).
static OBSERVERS_LIVE: AtomicBool = AtomicBool::new(false);

fn recheck_set() -> &'static Mutex<HashSet<u32>> {
    RECHECK.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Mark the snapshot dirty from outside the observer callback (e.g. the
/// gesture tap's drag-end reconcile).
pub fn mark_dirty() {
    DIRTY.store(true, Ordering::Relaxed);
}

/// Take-and-clear the dirty flag (ingest thread).
pub fn take_dirty() -> bool {
    DIRTY.swap(false, Ordering::Relaxed)
}

/// Drain the wids whose cached subrole should be re-resolved (ingest thread).
pub fn drain_recheck() -> Vec<u32> {
    let mut set = recheck_set().lock().expect("recheck lock poisoned");
    set.drain().collect()
}

pub fn observers_live() -> bool {
    OBSERVERS_LIVE.load(Ordering::Relaxed)
}

// ---- Observer callback (runs on the winsrv-ax runloop) --------------------

extern "C" fn observer_callback(
    _observer: *const c_void,
    element: *const c_void,
    notification: *const c_void,
    _refcon: *mut c_void,
) {
    // Minimal work only (plan §3 threading rules): flag + optional wid map.
    let name = unsafe { super::ax::cfstring_to_string_public(notification) }.unwrap_or_default();
    // TEMP #747 G3 diag (env-gated; delete with the fixing commit): which
    // notifications actually arrive, for which wid.
    if std::env::var_os("PETAL_AX_OBS_DIAG").is_some() {
        let wid = unsafe { super::ax::element_wid(element) };
        log::info!("AXOBS-DIAG notif={name} wid={wid:?}");
    }
    match classify_notification(&name) {
        ObserverAction::Ignore => {}
        ObserverAction::Lifecycle => {
            DIRTY.store(true, Ordering::Relaxed);
        }
        ObserverAction::StateRecheck => {
            DIRTY.store(true, Ordering::Relaxed);
            // Map element -> wid only via the private symbol (cheap, local).
            // Under correlation-only mode we skip targeted recheck — the sweep
            // still reconciles geometry, and staleness falls back to the
            // pre-observer behavior for that window.
            if let Some(wid) = unsafe { super::ax::element_wid(element) } {
                recheck_set()
                    .lock()
                    .expect("recheck lock poisoned")
                    .insert(wid);
            }
        }
    }
}

// ---- Hub: registration thread + command channel ---------------------------

enum Cmd {
    /// Full desired set of pids (the hub diffs internally).
    SyncApps(Vec<i32>),
}

pub struct AxObserverHub {
    tx: std::sync::mpsc::Sender<Cmd>,
    runloop: Mutex<Option<usize>>, // CFRunLoopRef as usize for Send
}

static HUB: OnceLock<AxObserverHub> = OnceLock::new();

/// Ask the hub to converge the observer set toward `pids` (ingest thread,
/// each tick; cheap — the hub diffs and no-ops when unchanged).
pub fn sync_apps(pids: Vec<i32>) {
    if let Some(hub) = HUB.get() {
        let _ = hub.tx.send(Cmd::SyncApps(pids));
        if let Some(rl) = *hub.runloop.lock().expect("runloop lock poisoned") {
            unsafe { CFRunLoopWakeUp(rl as *const c_void) };
        }
    }
}

/// Start the winsrv-ax thread (idempotent). Safe to call when untrusted —
/// registrations will fail per-app and the tier stays sweep-only.
pub fn start() {
    if HUB.get().is_some() {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
    let hub = AxObserverHub {
        tx,
        runloop: Mutex::new(None),
    };
    if HUB.set(hub).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("winsrv-ax".into())
        .spawn(move || unsafe {
            let rl = CFRunLoopGetCurrent();
            if let Some(hub) = HUB.get() {
                *hub.runloop.lock().expect("runloop lock poisoned") = Some(rl as usize);
            }
            // pid -> (observer, source, app element); torn down on unregister.
            let mut registered: HashMap<i32, (usize, usize, usize)> = HashMap::new();
            loop {
                // Drain all pending commands, then run the loop until woken.
                while let Ok(cmd) = rx.try_recv() {
                    let Cmd::SyncApps(pids) = cmd;
                    let should: HashSet<i32> = pids.into_iter().collect();
                    let have: HashSet<i32> = registered.keys().copied().collect();
                    let (add, remove) = diff_observer_apps(&should, &have);
                    for pid in remove {
                        if let Some((obs, src, app)) = registered.remove(&pid) {
                            CFRunLoopRemoveSource(
                                rl,
                                src as *const c_void,
                                kCFRunLoopDefaultMode,
                            );
                            CFRelease(obs as *const c_void);
                            CFRelease(app as *const c_void);
                        }
                    }
                    for pid in add {
                        let mut obs: *const c_void = std::ptr::null();
                        let create_err = AXObserverCreate(pid, observer_callback, &mut obs);
                        if std::env::var_os("PETAL_AX_OBS_DIAG").is_some() {
                            log::info!("AXOBS-DIAG register pid={pid} create_err={create_err}");
                        }
                        if create_err != 0 || obs.is_null() {
                            continue; // no AX server / untrusted -> sweep covers
                        }
                        let app = AXUIElementCreateApplication(pid);
                        if app.is_null() {
                            CFRelease(obs);
                            continue;
                        }
                        let mut any = false;
                        for name in NOTIFICATIONS {
                            let cf = super::ax::ax_attr_public(name);
                            if !cf.is_null() {
                                if AXObserverAddNotification(obs, app, cf, std::ptr::null_mut())
                                    == 0
                                {
                                    any = true;
                                }
                                CFRelease(cf);
                            }
                        }
                        if !any {
                            CFRelease(obs);
                            CFRelease(app);
                            continue;
                        }
                        let src = AXObserverGetRunLoopSource(obs);
                        CFRunLoopAddSource(rl, src, kCFRunLoopDefaultMode);
                        registered.insert(pid, (obs as usize, src as usize, app as usize));
                        OBSERVERS_LIVE.store(true, Ordering::Relaxed);
                    }
                }
                // Service observer sources for a bounded slice, then return to
                // drain the command channel. CFRunLoopRun() would BLOCK FOREVER
                // once sources exist -- the exact bug the G3 scripted gate
                // caught: the first SyncApps batch registered, every later app
                // (each freshly-spawned probe) starved unregistered, so no
                // state events, no recheck, stale Popup forever. WakeUp from
                // sync_apps() shortens the slice further when commands arrive.
                // And kCFRunLoopRunFinished (=1, no sources yet) returns
                // IMMEDIATELY -- sleep-pace or this spins a core (the
                // winsrv-sls 100%-CPU live finding; latent here pre-register).
                if CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.25, false) == 1 {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_names_map_to_the_right_actions() {
        assert_eq!(classify_notification("AXWindowCreated"), ObserverAction::Lifecycle);
        assert_eq!(classify_notification("AXUIElementDestroyed"), ObserverAction::Lifecycle);
        assert_eq!(classify_notification("AXWindowMoved"), ObserverAction::StateRecheck);
        assert_eq!(classify_notification("AXWindowResized"), ObserverAction::StateRecheck);
        assert_eq!(classify_notification("AXWindowMiniaturized"), ObserverAction::StateRecheck);
        assert_eq!(
            classify_notification("AXWindowDeminiaturized"),
            ObserverAction::StateRecheck
        );
        assert_eq!(classify_notification("AXTitleChanged"), ObserverAction::Ignore);
    }

    #[test]
    fn app_diffing_registers_new_and_drops_gone() {
        let should: HashSet<i32> = [1, 2, 3].into_iter().collect();
        let have: HashSet<i32> = [2, 4].into_iter().collect();
        let (add, remove) = diff_observer_apps(&should, &have);
        assert_eq!(add, vec![1, 3]);
        assert_eq!(remove, vec![4]);
        // converged -> no-ops
        let (add2, remove2) = diff_observer_apps(&should, &should);
        assert!(add2.is_empty() && remove2.is_empty());
    }

    #[test]
    fn recheck_drain_clears_and_dedupes() {
        recheck_set().lock().unwrap().clear();
        recheck_set().lock().unwrap().extend([10u32, 10, 11]);
        let mut drained = drain_recheck();
        drained.sort_unstable();
        assert_eq!(drained, vec![10, 11]);
        assert!(drain_recheck().is_empty(), "drain clears");
    }
}
