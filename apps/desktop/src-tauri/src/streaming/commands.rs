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

/// A close request on the dialog: with `hide_instead` (macOS) it is hidden
/// and the close is prevented, so the window is reused. True when prevented.
fn on_close_requested<D: GoLiveDialog>(dialog: &D, hide_instead: bool) -> bool {
    if hide_instead {
        dialog.hide();
    }
    hide_instead
}

impl GoLiveDialog for tauri::WebviewWindow {
    fn retarget(&self, window_id: u32) {
        let _ = tauri::Emitter::emit(self, "go-live-target", window_id);
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
