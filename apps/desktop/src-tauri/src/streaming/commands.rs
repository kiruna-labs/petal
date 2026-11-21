//! Tauri commands for live streaming. Stream keys flow one way only: the
//! go-live dialog may send one in, nothing ever sends one out.

use tauri::AppHandle;

use super::destinations::{DestinationStore, SavedDestination};
use super::keystore::platform_store;
use super::plan::{StreamOrientation, StreamResolution};
use super::session::{GoLiveRequest, StopReason, StreamPreview, StreamSessionSnapshot};

pub const GO_LIVE_WINDOW_LABEL: &str = "go-live";

fn store(app: &AppHandle) -> Result<DestinationStore, String> {
    let dir = tauri::Manager::path(app).app_data_dir().map_err(|e| e.to_string())?;
    Ok(DestinationStore::new(&dir, platform_store()))
}

#[tauri::command]
pub fn stream_destinations_list(app: AppHandle) -> Result<Vec<SavedDestination>, String> {
    Ok(store(&app)?.list())
}

#[tauri::command]
pub fn stream_destination_delete(app: AppHandle, id: String) -> Result<(), String> {
    store(&app)?.delete(&id).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn stream_preview(
    app: AppHandle,
    window_id: u32,
    resolution: StreamResolution,
    orientation: Option<StreamOrientation>,
    max_bitrate_bps: Option<u32>,
) -> Result<StreamPreview, String> {
    super::session::preview(&app, window_id, resolution, orientation.unwrap_or_default(), max_bitrate_bps).await
}

#[tauri::command]
pub async fn stream_go_live(app: AppHandle, request: GoLiveRequest) -> Result<StreamSessionSnapshot, String> {
    super::session::go_live(&app, request).await
}

#[tauri::command]
pub async fn stream_stop(app: AppHandle) {
    super::session::stop(&app, StopReason::User).await;
}

#[tauri::command]
pub async fn stream_resume_here(app: AppHandle, window_id: u32) -> Result<(), String> {
    super::session::resume_here(&app, window_id).await
}

#[tauri::command]
pub fn stream_session_state() -> Option<StreamSessionSnapshot> {
    super::session::current()
}

fn go_live_target_slot() -> &'static std::sync::atomic::AtomicU32 {
    static TARGET: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    &TARGET
}

/// The share the go-live dialog is for (0 = none yet).
#[tauri::command]
pub fn go_live_target() -> u32 {
    go_live_target_slot().load(std::sync::atomic::Ordering::Relaxed)
}

/// The go-live dialog: a singleton window, re-targeted if already open.
/// Async because a sync command building a window deadlocks on Windows.
///
/// On macOS it is hidden, never destroyed: the dialog closes itself the
/// moment `stream_go_live` answers, and WebKit crashed (a null scrolling
/// coordinator in `RemoteLayerTreeDrawingAreaProxyMac::didCommitLayerTree`)
/// on a layer commit that arrived for the page being torn down. Same
/// hide-and-reuse rule as the compositor and border panels.
#[tauri::command]
pub async fn open_go_live_window(app: AppHandle, window_id: u32) -> Result<(), String> {
    open_go_live(&TauriDialogHost { app: &app }, window_id, HIDE_ON_CLOSE)
}

/// macOS hides the dialog on close and reuses it; elsewhere it is destroyed
/// and the next open builds a fresh one.
const HIDE_ON_CLOSE: bool = cfg!(target_os = "macos");

/// The go-live dialog's window, as opening and closing it uses it. The
/// lifecycle test drives [`open_go_live`] through this seam with a fake that
/// records every call and keeps the close handler production registers, the
/// same pattern as hover_tab's `HoverTabPanelHandle`: tauri's `MockRuntime`
/// show/hide are no-ops that record nothing. `tauri::WebviewWindow`
/// implements it as passthroughs.
pub(crate) trait GoLiveDialog: Clone + Send + 'static {
    /// Tell the page which share it is for now (`go-live-target`).
    fn retarget(&self, window_id: u32);
    /// Tell the page it is closing (`go-live-closed`), so it drops the typed
    /// stream key before the window is hidden or destroyed.
    fn closing(&self);
    fn bring_forward(&self);
    fn hide(&self);
    /// Call `prevent` on every close request (the page's own close() and the
    /// title bar's close button); the close is prevented when it returns true.
    fn on_close_request(&self, prevent: impl Fn() -> bool + Send + 'static);
}

/// Where the go-live dialog's window comes from: the one already open (a
/// hidden one included), or a new one.
pub(crate) trait GoLiveDialogHost {
    type Dialog: GoLiveDialog;
    fn existing(&self) -> Option<Self::Dialog>;
    fn create(&self) -> Result<Self::Dialog, String>;
}

/// Open the dialog for share `window_id`: the one that exists (on macOS a
/// closed dialog is only hidden) is re-targeted and brought forward, else a
/// new one is built, with its close handler.
fn open_go_live<H: GoLiveDialogHost>(host: &H, window_id: u32, hide_on_close: bool) -> Result<(), String> {
    go_live_target_slot().store(window_id, std::sync::atomic::Ordering::Relaxed);
    if let Some(dialog) = host.existing() {
        dialog.retarget(window_id);
        dialog.bring_forward();
        return Ok(());
    }
    let dialog = host.create()?;
    let closing = dialog.clone();
    dialog.on_close_request(move || on_close_requested(&closing, hide_on_close));
    Ok(())
}

/// A close request on the dialog (every way it closes: Cancel, after going
/// live, the title bar): the page drops the typed key first, then with
/// `hide_instead` (macOS) it is hidden and the close is prevented, so the
/// window is reused. True when prevented.
fn on_close_requested<D: GoLiveDialog>(dialog: &D, hide_instead: bool) -> bool {
    dialog.closing();
    if hide_instead {
        dialog.hide();
    }
    hide_instead
}

impl GoLiveDialog for tauri::WebviewWindow {
    fn retarget(&self, window_id: u32) {
        let _ = tauri::Emitter::emit(self, "go-live-target", window_id);
    }
    fn closing(&self) {
        let _ = tauri::Emitter::emit(self, "go-live-closed", ());
    }
    fn bring_forward(&self) {
        let _ = self.show();
        let _ = self.unminimize();
        let _ = self.set_focus();
    }
    fn hide(&self) {
        let _ = tauri::WebviewWindow::hide(self);
    }
    fn on_close_request(&self, prevent: impl Fn() -> bool + Send + 'static) {
        self.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if prevent() {
                    api.prevent_close();
                }
            }
        });
    }
}

struct TauriDialogHost<'a> {
    app: &'a AppHandle,
}

impl GoLiveDialogHost for TauriDialogHost<'_> {
    type Dialog = tauri::WebviewWindow;

    fn existing(&self) -> Option<tauri::WebviewWindow> {
        tauri::Manager::get_webview_window(self.app, GO_LIVE_WINDOW_LABEL)
    }

    fn create(&self) -> Result<tauri::WebviewWindow, String> {
        use tauri::{WebviewUrl, WebviewWindowBuilder};
        use crate::streaming::streamer_mode::BuildPetalWindow;
        let builder =
            WebviewWindowBuilder::new(self.app, GO_LIVE_WINDOW_LABEL, WebviewUrl::App("go-live.html".into()))
                .title("Go live")
                .inner_size(460.0, 660.0)
                .min_inner_size(400.0, 520.0)
                // Opened from a share's menu, so Petal is usually inactive (sharing
                // hands focus back to the source app): without this macOS spends
                // the first click on activating Petal, like the meeting window.
                .accept_first_mouse(true)
                // Hidden until loaded: a visible HWND paints white before WebView2.
                .visible(false)
                .on_page_load(|window, payload| {
                    if matches!(payload.event(), tauri::webview::PageLoadEvent::Finished) {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                });
        #[cfg(target_os = "windows")]
        let builder = builder.additional_browser_args(crate::webview2_args::WEBVIEW2_ACCEL_ARGS);
        builder.build_petal().map_err(|error| format!("couldn't open the go-live window: {error}"))
    }
}

/// The live-stream entry a share's options menu shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LiveMenuAction {
    /// "Go live…": shared, and nothing is live in the room.
    GoLive,
    /// "Stop live stream": this is the live share.
    Stop,
    /// "Resume live stream here": our stream is on its slate.
    Resume,
    /// "Switch live stream here": our stream is live on another share.
    Switch,
}

/// `session`: our own stream's (live window, on slate), the window `None`
/// once its share stopped; `room_live`: anything live in the room (ours
/// included).
pub fn live_menu_action(
    window_id: u32,
    shared: bool,
    session: Option<(Option<u32>, bool)>,
    room_live: bool,
) -> Option<LiveMenuAction> {
    if !shared {
        return None;
    }
    match session {
        Some((Some(live_window), _)) if live_window == window_id => Some(LiveMenuAction::Stop),
        Some((_, true)) => Some(LiveMenuAction::Resume),
        Some((_, false)) => Some(LiveMenuAction::Switch),
        None if room_live => None,
        None => Some(LiveMenuAction::GoLive),
    }
}

fn menu_action_for(app: &AppHandle, window_id: u32) -> Option<LiveMenuAction> {
    // Client mode encodes on Windows and macOS; elsewhere the entry stays
    // hidden rather than leading to a dialog that can't go live.
    if !cfg!(any(target_os = "windows", target_os = "macos")) {
        return None;
    }
    let shared = tauri::Manager::state::<crate::session::SessionState>(app).is_share_active(window_id);
    // A stopped live share leaves no live window: every share, the same
    // window shared again included, offers the resume.
    let ended = super::tap::live_share_ended();
    let session = super::session::current().map(|s| {
        ((!ended).then_some(s.window_id), ended || s.phase == super::session::SessionPhase::Paused)
    });
    live_menu_action(window_id, shared, session, super::live_state::current().live)
}

#[tauri::command]
pub fn stream_menu_action(app: AppHandle, window_id: u32) -> Option<LiveMenuAction> {
    menu_action_for(&app, window_id)
}

/// Petal View: the same, addressed by selector label (capture tokens never
/// cross into that route).
#[tauri::command]
pub fn region_stream_menu_action(app: AppHandle, window_label: String) -> Option<LiveMenuAction> {
    let token = crate::region_window::ensure_region_token(&app, &window_label).ok()?;
    menu_action_for(&app, token)
}

#[tauri::command]
pub async fn region_stream_action(
    app: AppHandle,
    window_label: String,
    action: LiveMenuAction,
) -> Result<(), String> {
    let token = crate::region_window::ensure_region_token(&app, &window_label)?;
    match action {
        LiveMenuAction::GoLive => open_go_live_window(app, token).await,
        LiveMenuAction::Stop => {
            super::session::stop(&app, StopReason::User).await;
            Ok(())
        }
        LiveMenuAction::Resume | LiveMenuAction::Switch => super::session::resume_here(&app, token).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type CloseHandler = Box<dyn Fn() -> bool + Send>;

    /// A dialog window that records what the lifecycle does to it and keeps
    /// the close handler production registers on it.
    #[derive(Default)]
    struct FakeWindow {
        calls: Mutex<Vec<String>>,
        visible: Mutex<bool>,
        close_handlers: Mutex<Vec<CloseHandler>>,
    }

    #[derive(Clone, Default)]
    struct FakeDialog(Arc<FakeWindow>);

    impl GoLiveDialog for FakeDialog {
        fn retarget(&self, window_id: u32) {
            self.0.calls.lock().unwrap().push(format!("retarget {window_id}"));
        }
        fn closing(&self) {
            self.0.calls.lock().unwrap().push("closing".into());
        }
        fn bring_forward(&self) {
            self.0.calls.lock().unwrap().push("bring_forward".into());
            *self.0.visible.lock().unwrap() = true;
        }
        fn hide(&self) {
            self.0.calls.lock().unwrap().push("hide".into());
            *self.0.visible.lock().unwrap() = false;
        }
        fn on_close_request(&self, prevent: impl Fn() -> bool + Send + 'static) {
            self.0.close_handlers.lock().unwrap().push(Box::new(prevent));
        }
    }

    impl FakeDialog {
        fn visible(&self) -> bool {
            *self.0.visible.lock().unwrap()
        }
        fn calls(&self) -> Vec<String> {
            self.0.calls.lock().unwrap().clone()
        }
        fn handlers(&self) -> usize {
            self.0.close_handlers.lock().unwrap().len()
        }
    }

    /// The app's window registry, and the platform: a close request runs
    /// every handler registered on the window, and a close none of them
    /// prevented destroys it.
    #[derive(Default)]
    struct FakeHost {
        window: Mutex<Option<FakeDialog>>,
        builds: Mutex<u32>,
    }

    impl GoLiveDialogHost for FakeHost {
        type Dialog = FakeDialog;
        fn existing(&self) -> Option<FakeDialog> {
            self.window.lock().unwrap().clone()
        }
        fn create(&self) -> Result<FakeDialog, String> {
            *self.builds.lock().unwrap() += 1;
            // Shown once its page loads (`on_page_load`).
            let dialog = FakeDialog::default();
            *dialog.0.visible.lock().unwrap() = true;
            *self.window.lock().unwrap() = Some(dialog.clone());
            Ok(dialog)
        }
    }

    impl FakeHost {
        fn builds(&self) -> u32 {
            *self.builds.lock().unwrap()
        }
        fn window(&self) -> Option<FakeDialog> {
            self.window.lock().unwrap().clone()
        }
        /// The page's own close() (Cancel, or after `stream_go_live`
        /// answers) or the title bar: the platform's close request.
        fn request_close(&self) {
            let window = self.window().expect("a dialog to close");
            let handlers = window.0.close_handlers.lock().unwrap();
            let prevented = handlers.iter().fold(false, |prevented, handler| handler() || prevented);
            drop(handlers);
            if !prevented {
                *self.window.lock().unwrap() = None;
            }
        }
    }

    #[test]
    fn the_go_live_dialog_opens_closes_and_reopens_through_its_handlers() {
        for hide_on_close in [true, false] {
            let platform = if hide_on_close { "macOS" } else { "Windows" };
            let host = FakeHost::default();
            // Open: one window, for share 7, with one close handler.
            open_go_live(&host, 7, hide_on_close).unwrap();
            assert_eq!(host.builds(), 1, "{platform}: built once");
            assert_eq!(go_live_target(), 7, "{platform}: the page reads its share on load");
            let first = host.window().unwrap();
            assert_eq!(first.handlers(), 1, "{platform}: one close handler");
            // Opened again from another share while open: the same window,
            // re-targeted and brought forward, never a second one or handler.
            open_go_live(&host, 9, hide_on_close).unwrap();
            assert_eq!(host.builds(), 1, "{platform}: still one window");
            assert_eq!(first.calls(), ["retarget 9", "bring_forward"], "{platform}");
            assert_eq!(first.handlers(), 1, "{platform}: still one handler");
            assert_eq!(go_live_target(), 9);
            // Cancel (or the title bar): the platform's close request.
            host.request_close();
            assert_eq!(
                first.calls().iter().find(|call| *call == "closing" || *call == "hide").map(String::as_str),
                Some("closing"),
                "{platform}: the page drops the key before the window is hidden or destroyed"
            );
            if hide_on_close {
                assert!(!first.visible(), "macOS: hidden");
                assert!(host.window().is_some(), "macOS: not destroyed");
            } else {
                assert!(host.window().is_none(), "Windows: destroyed");
                assert!(!first.calls().contains(&"hide".to_string()), "Windows: never hidden instead");
            }
            // Reopened for share 11: no stale window. macOS brings the hidden
            // one back re-targeted (the page resets on that event); Windows
            // builds a fresh one, which reads its share on load.
            open_go_live(&host, 11, hide_on_close).unwrap();
            assert_eq!(go_live_target(), 11);
            let reopened = host.window().unwrap();
            assert!(reopened.visible(), "{platform}: shown again");
            assert_eq!(reopened.handlers(), 1, "{platform}: one close handler");
            if hide_on_close {
                assert_eq!(host.builds(), 1, "macOS: the hidden dialog is reused");
                assert!(Arc::ptr_eq(&reopened.0, &first.0));
                assert_eq!(first.calls().last().map(String::as_str), Some("bring_forward"));
                assert!(first.calls().contains(&"retarget 11".to_string()));
            } else {
                assert_eq!(host.builds(), 2, "Windows: a fresh window");
                assert!(!Arc::ptr_eq(&reopened.0, &first.0));
            }
            // Going live: the page closes itself once `stream_go_live`
            // answers (tests/goLiveDialogLifecycle.test.ts), the same close
            // request, so the dialog retires the same way.
            host.request_close();
            assert_eq!(host.window().is_some(), hide_on_close, "{platform}: retired after going live");
            if hide_on_close {
                assert!(!reopened.visible(), "macOS: hidden again");
            }
        }
    }

    /// The `tauri::WebviewWindow` glue can't run in a unit test, so pin it to
    /// what the lifecycle test drives: the command is `open_go_live` with the
    /// real host and platform flag, and the real window's close handler runs
    /// on its `CloseRequested` event, prevented only when the handler says so.
    #[test]
    fn the_go_live_command_and_window_go_through_the_tested_path() {
        let source = include_str!("commands.rs");
        let command_at = source.find("pub async fn open_go_live_window(").expect("the command exists");
        let command = &source[command_at..command_at + source[command_at..].find("\n}\n").expect("its end")];
        assert!(
            command.contains("open_go_live(&TauriDialogHost { app: &app }, window_id, HIDE_ON_CLOSE)"),
            "the command is open_go_live with the real host: {command}"
        );
        assert_eq!(command.lines().count(), 2, "and nothing else: {command}");
        let impl_at = source.find("impl GoLiveDialog for tauri::WebviewWindow {").expect("the real window");
        let window = &source[impl_at..impl_at + source[impl_at..].find("\n}\n").expect("its end")];
        let close = &window[window.find("fn on_close_request(").expect("registers the close handler")..];
        let registered = close.find("self.on_window_event(").expect("on the window's events");
        let event = close.find("WindowEvent::CloseRequested").expect("on the close request");
        let asked = close.find("if prevent()").expect("asks the handler");
        let prevented = close.find("api.prevent_close()").expect("prevents the close");
        assert!(registered < event && event < asked && asked < prevented, "prevented only when the handler says so: {close}");
        // The events the page listens for, by the names it listens for them.
        let ipc = include_str!("../../../src/lib/ipc.ts");
        for (method, ipc_name, wire) in [
            ("fn retarget(", "goLiveTarget: ", "go-live-target"),
            ("fn closing(", "goLiveClosed: ", "go-live-closed"),
        ] {
            let at = window.find(method).unwrap_or_else(|| panic!("the real window has {method}"));
            let emit = format!("tauri::Emitter::emit(self, \"{wire}\"");
            assert!(window[at..].split("
    }").next().unwrap().contains(&emit), "{method} emits {wire}");
            assert!(ipc.contains(&format!("{ipc_name}'{wire}'")), "ipc.ts EVENTS.{ipc_name} is {wire}");
        }
        assert!(HIDE_ON_CLOSE == cfg!(target_os = "macos"));
    }


    #[test]
    fn menu_shows_the_one_action_that_fits() {
        // Not shared: nothing.
        assert_eq!(live_menu_action(7, false, None, false), None);
        // Shared, nothing live: Go live.
        assert_eq!(live_menu_action(7, true, None, false), Some(LiveMenuAction::GoLive));
        // Someone else (or a server stream) is live: one stream per room.
        assert_eq!(live_menu_action(7, true, None, true), None);
        // This is our live share: Stop.
        assert_eq!(live_menu_action(7, true, Some((Some(7), false)), true), Some(LiveMenuAction::Stop));
        assert_eq!(live_menu_action(7, true, Some((Some(7), true)), true), Some(LiveMenuAction::Stop));
        // Our stream is on its slate: any other share can resume it.
        assert_eq!(live_menu_action(9, true, Some((Some(7), true)), true), Some(LiveMenuAction::Resume));
        // Its share stopped: the same window shared again resumes too.
        assert_eq!(live_menu_action(7, true, Some((None, true)), true), Some(LiveMenuAction::Resume));
        // Our stream is live elsewhere: any other share can take it over.
        assert_eq!(live_menu_action(9, true, Some((Some(7), false)), true), Some(LiveMenuAction::Switch));
        // Not shared: still nothing, live or not.
        assert_eq!(live_menu_action(9, false, Some((Some(7), false)), true), None);
    }
}
