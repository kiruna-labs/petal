//! Passive viewer-demand signaling for shared-window quality.
//!
//! Receivers publish one tiny data-channel message while a remote compositor
//! window is open/visible. The sharer treats that as live demand and keeps the
//! matching share at `Full` even if it is not the sharer's most-recent share.

#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use livekit::prelude::*;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::session::{RoomGeneration, SessionState, ViewerDemandEvent, ViewerDemandUpdate};

pub const TOPIC: &str = "petal.viewer-demand";
const WIRE_VERSION: u8 = 2;
const MAX_DEMAND_DIMENSION_PX: u32 = crate::transport::publisher::VIDEO_TOOLBOX_H264_MAX_LONG_EDGE;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const EXPIRY_INTERVAL: Duration = Duration::from_secs(2);
const GEOMETRY_REFRESH_DEBOUNCE: Duration = Duration::from_millis(150);
const LOWEST_LAYER_WIDTH: u32 = 640;
const LOWEST_LAYER_HEIGHT: u32 = 360;
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);
static NEXT_GEOMETRY_REFRESH: AtomicU64 = AtomicU64::new(1);
static GEOMETRY_REFRESH_TASKS: OnceLock<
    Mutex<HashMap<u32, (u64, tauri::async_runtime::JoinHandle<()>)>>,
> = OnceLock::new();
static OCCLUSION_STATE: OnceLock<Mutex<HashMap<u32, OcclusionHysteresis>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, Default)]
struct OcclusionHysteresis {
    consecutive_occluded: u8,
    downgraded: bool,
}

fn update_occlusion_hysteresis(state: &mut OcclusionHysteresis, observed_occluded: bool) -> bool {
    if observed_occluded {
        state.consecutive_occluded = state.consecutive_occluded.saturating_add(1);
        if state.consecutive_occluded >= 2 {
            state.downgraded = true;
        }
    } else {
        *state = OcclusionHysteresis::default();
    }
    state.downgraded
}

fn occlusion_is_debounced(window_id: u32, observed_occluded: bool) -> bool {
    let mut states = OCCLUSION_STATE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let state = states.entry(window_id).or_default();
    update_occlusion_hysteresis(state, observed_occluded)
}

fn clear_occlusion_state(window_id: u32) {
    if let Some(states) = OCCLUSION_STATE.get() {
        states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&window_id);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ViewerDemandKind {
    Open,
    Closed,
    Heartbeat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewerDemandMessage {
    #[serde(default = "legacy_wire_version")]
    pub v: u8,
    pub kind: ViewerDemandKind,
    pub target_user_id: String,
    pub viewer_id: String,
    pub window_id: u32,
    pub seq: u64,
    pub visible: bool,
    pub width: u32,
    pub height: u32,
    #[serde(default = "default_receiver_scale")]
    pub scale: f64,
    #[serde(default)]
    pub pixel_width: u32,
    #[serde(default)]
    pub pixel_height: u32,
    /// A receiver-side no-frame watchdog retired its window but still wants
    /// the owner to repair the publication. A real user close leaves this
    /// false, so the owner can distinguish repair from give-up.
    #[serde(default)]
    pub needs_republish: bool,
}

const fn legacy_wire_version() -> u8 {
    1
}

const fn default_receiver_scale() -> f64 {
    1.0
}

fn normalized_geometry(message: &ViewerDemandMessage) -> (f64, u32, u32) {
    let scale = if message.v < WIRE_VERSION {
        1.0
    } else if message.scale.is_finite() && message.scale > 0.0 {
        message.scale.clamp(0.5, 4.0)
    } else {
        1.0
    };
    let derived_width = (f64::from(message.width) * scale).ceil() as u32;
    let derived_height = (f64::from(message.height) * scale).ceil() as u32;
    let pixel_width = if message.v >= WIRE_VERSION && message.pixel_width > 0 {
        message.pixel_width
    } else {
        derived_width
    }
    .min(MAX_DEMAND_DIMENSION_PX);
    let pixel_height = if message.v >= WIRE_VERSION && message.pixel_height > 0 {
        message.pixel_height
    } else {
        derived_height
    }
    .min(MAX_DEMAND_DIMENSION_PX);
    (scale, pixel_width, pixel_height)
}

impl From<ViewerDemandKind> for ViewerDemandEvent {
    fn from(kind: ViewerDemandKind) -> Self {
        match kind {
            ViewerDemandKind::Open => Self::Open,
            ViewerDemandKind::Closed => Self::Closed,
            ViewerDemandKind::Heartbeat => Self::Heartbeat,
        }
    }
}

pub fn start_for_room(
    app: &AppHandle,
    room: Arc<Room>,
    local_identity: String,
    generation: RoomGeneration,
) {
    start_receiver(
        app,
        room.clone(),
        local_identity.clone(),
        generation.clone(),
    );
    start_heartbeat_sender(app, local_identity, generation.clone());
    start_expiry_loop(app, generation);
}

fn start_receiver(
    app: &AppHandle,
    room: Arc<Room>,
    local_identity: String,
    generation: RoomGeneration,
) {
    let mut events = room.subscribe();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                log::debug!("viewer-demand: receiver exiting for stale room generation");
                break;
            }
            let RoomEvent::DataReceived {
                payload,
                topic,
                participant,
                ..
            } = event
            else {
                continue;
            };
            if topic.as_deref() != Some(TOPIC) {
                continue;
            }
            let Ok(mut message) = serde_json::from_slice::<ViewerDemandMessage>(&payload) else {
                log::debug!("viewer-demand: dropping invalid JSON payload");
                continue;
            };
            if message.target_user_id != local_identity {
                continue;
            }
            let Some(sender_identity) = participant.as_ref().map(|p| p.identity().to_string())
            else {
                log::debug!("viewer-demand: dropping anonymous demand packet");
                continue;
            };
            if message.viewer_id != sender_identity {
                log::warn!(
                    "viewer-demand: viewerId '{}' did not match packet sender '{}'; using trusted sender",
                    message.viewer_id,
                    sender_identity
                );
                message.viewer_id = sender_identity;
            }
            let Some(state) = app.try_state::<SessionState>() else {
                continue;
            };
            let window_id = message.window_id;
            let event = ViewerDemandEvent::from(message.kind);
            let (scale, pixel_width, pixel_height) = normalized_geometry(&message);
            let accepted = crate::session::note_passive_viewer_demand(
                state.inner(),
                ViewerDemandUpdate {
                    event,
                    viewer_id: message.viewer_id.clone(),
                    window_id,
                    seq: message.seq,
                    visible: message.visible,
                    width: message.width,
                    height: message.height,
                    scale,
                    pixel_width,
                    pixel_height,
                    received_at: Instant::now(),
                },
            );
            if !accepted {
                continue;
            }
            log::debug!(
                "viewer-demand: {:?} for local window {} from '{}' visible={} logical={}x{} scale={:.2} pixels={}x{} seq={}",
                message.kind,
                window_id,
                message.viewer_id,
                message.visible,
                message.width,
                message.height,
                scale,
                pixel_width,
                pixel_height,
                message.seq
            );
            if message.needs_republish {
                crate::session::repair_active_share_publication(state.inner(), window_id).await;
            } else {
                crate::session::reconcile_quality_for_window(state.inner(), window_id).await;
            }
        }
    });
}

fn start_heartbeat_sender(app: &AppHandle, local_identity: String, generation: RoomGeneration) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        loop {
            interval.tick().await;
            if !generation.is_current() {
                log::debug!("viewer-demand: heartbeat sender exiting for stale room generation");
                break;
            }
            for demand in open_window_demands(&app, &local_identity, ViewerDemandKind::Heartbeat) {
                publish_message(&app, demand);
            }
        }
    });
}

fn start_expiry_loop(app: &AppHandle, generation: RoomGeneration) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(EXPIRY_INTERVAL);
        loop {
            interval.tick().await;
            if !generation.is_current() {
                log::debug!("viewer-demand: expiry loop exiting for stale room generation");
                break;
            }
            let Some(state) = app.try_state::<SessionState>() else {
                continue;
            };
            let expired =
                crate::session::expire_stale_viewer_demands(state.inner(), Instant::now());
            for window_id in expired {
                crate::session::reconcile_quality_for_window(state.inner(), window_id).await;
            }
        }
    });
}

pub fn publish_window_open(app: &AppHandle, window_id: u32) {
    cancel_window_geometry_refresh(window_id);
    publish_window_event(app, window_id, ViewerDemandKind::Open);
}

pub fn publish_window_closed(app: &AppHandle, window_id: u32) {
    cancel_window_geometry_refresh(window_id);
    publish_window_event(app, window_id, ViewerDemandKind::Closed);
}

/// Ask the owner to republish after the receiver retired a frozen window.
/// This deliberately travels on the existing demand heartbeat channel so it
/// has the same room-generation and viewer identity guarantees as demand.
pub fn publish_window_repair_request(app: &AppHandle, window_id: u32) {
    cancel_window_geometry_refresh(window_id);
    publish_window_event_with_repair(app, window_id, ViewerDemandKind::Heartbeat, true);
}

pub fn schedule_window_geometry_refresh(app: &AppHandle, window_id: u32) {
    let token = NEXT_GEOMETRY_REFRESH.fetch_add(1, Ordering::Relaxed);
    let app = app.clone();
    let task = tauri::async_runtime::spawn(async move {
        tokio::time::sleep(GEOMETRY_REFRESH_DEBOUNCE).await;
        let is_current = GEOMETRY_REFRESH_TASKS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .ok()
            .and_then(|tasks| tasks.get(&window_id).map(|(current, _)| *current == token))
            .unwrap_or(false);
        if !is_current {
            return;
        }
        publish_window_event(&app, window_id, ViewerDemandKind::Heartbeat);
        if let Ok(mut tasks) = GEOMETRY_REFRESH_TASKS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
        {
            if tasks
                .get(&window_id)
                .is_some_and(|(current, _)| *current == token)
            {
                tasks.remove(&window_id);
            }
        }
    });
    let old = GEOMETRY_REFRESH_TASKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()
        .and_then(|mut tasks| tasks.insert(window_id, (token, task)));
    if let Some((_, old)) = old {
        old.abort();
    }
}

fn cancel_window_geometry_refresh(window_id: u32) {
    let task = GEOMETRY_REFRESH_TASKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()
        .and_then(|mut tasks| tasks.remove(&window_id));
    if let Some((_, task)) = task {
        task.abort();
    }
}

fn publish_window_event(app: &AppHandle, window_id: u32, kind: ViewerDemandKind) {
    publish_window_event_with_repair(app, window_id, kind, false);
}

fn publish_window_event_with_repair(
    app: &AppHandle,
    window_id: u32,
    kind: ViewerDemandKind,
    needs_republish: bool,
) {
    let Some((_, local_identity)) = app
        .try_state::<SessionState>()
        .and_then(|state| state.control_channel_snapshot())
    else {
        // Fable nit: a window can be torn down (Closed) after the control
        // channel is already gone (e.g. room disconnect) — there is nothing
        // to publish, but local occlusion-hysteresis state must still be
        // dropped so it doesn't linger for this window_id indefinitely.
        if kind == ViewerDemandKind::Closed {
            clear_occlusion_state(window_id);
        }
        return;
    };
    let Some(mut demand) = demand_for_window(app, &local_identity, window_id, kind) else {
        return;
    };
    demand.needs_republish = needs_republish;
    publish_message(app, demand);
}

fn open_window_demands(
    app: &AppHandle,
    local_identity: &str,
    kind: ViewerDemandKind,
) -> Vec<ViewerDemandMessage> {
    crate::compositor::open_content_frames(app)
        .into_iter()
        .filter_map(|(window_id, _)| demand_for_window(app, local_identity, window_id, kind))
        .collect()
}

fn demand_for_window(
    app: &AppHandle,
    local_identity: &str,
    window_id: u32,
    kind: ViewerDemandKind,
) -> Option<ViewerDemandMessage> {
    let target_user_id = crate::compositor::owner_identity_for_window(window_id, None)?;
    if target_user_id == local_identity {
        return None;
    }
    let geometry = crate::compositor::content_frame_and_scale_for_window(app, window_id);
    let first_frame_seen = crate::compositor::source_pixel_size_for_window(window_id).is_some();
    let appkit_reports_occluded = if kind == ViewerDemandKind::Closed {
        clear_occlusion_state(window_id);
        false
    } else {
        crate::compositor::window_is_fully_occluded(app, window_id)
    };
    let (visible, width, height, scale) = match (kind, geometry) {
        (ViewerDemandKind::Closed, Some((frame, scale))) => (
            false,
            frame.width.max(0) as u32,
            frame.height.max(0) as u32,
            scale,
        ),
        (ViewerDemandKind::Closed, None) => (false, 0, 0, 1.0),
        (_, Some((frame, scale))) => (
            frame.width > 0 && frame.height > 0,
            frame.width.max(0) as u32,
            frame.height.max(0) as u32,
            scale,
        ),
        (_, None) => (false, 0, 0, 1.0),
    };
    let (pixel_width, pixel_height) =
        demand_pixel_dimensions(width, height, scale, first_frame_seen);
    let (requested_width, requested_height) = startup_demand_decision(
        window_id,
        StartupDemandInputs {
            closing: kind == ViewerDemandKind::Closed,
            geometry_visible: visible,
            appkit_reports_occluded,
            first_frame_seen,
            pixel_width,
            pixel_height,
        },
    );
    if kind != ViewerDemandKind::Closed {
        crate::transport::subscriber::update_window_subscription_dimensions(
            &target_user_id,
            window_id,
            requested_width.min(MAX_DEMAND_DIMENSION_PX),
            requested_height.min(MAX_DEMAND_DIMENSION_PX),
        );
    }
    Some(ViewerDemandMessage {
        v: WIRE_VERSION,
        kind,
        target_user_id,
        viewer_id: local_identity.to_string(),
        window_id,
        seq: NEXT_SEQ.fetch_add(1, Ordering::Relaxed),
        visible,
        width,
        height,
        scale,
        pixel_width: pixel_width.min(MAX_DEMAND_DIMENSION_PX),
        pixel_height: pixel_height.min(MAX_DEMAND_DIMENSION_PX),
        needs_republish: false,
    })
}

/// Before the first decoded frame, the panel's dimensions are only a
/// 640x400 placeholder. Advertising those pixels would select a small
/// simulcast layer, which then feeds the placeholder-sized window back into
/// this demand path. Start at the maximum supported demand until the source
/// dimensions are known; subsequent geometry refreshes can lower it normally.
fn demand_pixel_dimensions(
    width: u32,
    height: u32,
    scale: f64,
    has_source_pixel_size: bool,
) -> (u32, u32) {
    if !has_source_pixel_size {
        return (MAX_DEMAND_DIMENSION_PX, MAX_DEMAND_DIMENSION_PX);
    }
    (
        (f64::from(width) * scale).ceil() as u32,
        (f64::from(height) * scale).ceil() as u32,
    )
}

fn requested_dimensions(visible: bool, width: u32, height: u32) -> (u32, u32) {
    if visible {
        (width, height)
    } else {
        (LOWEST_LAYER_WIDTH, LOWEST_LAYER_HEIGHT)
    }
}

/// Everything `demand_for_window` knows when it decides which simulcast layer
/// to ask the SFU for. Split out so `examples/startup_layer_probe` can drive
/// the REAL decision -- occlusion hysteresis and its cross-call state included
/// -- against a live SFU. A unit test on the pure pieces cannot show which
/// layer the SFU actually hands back, or for how long (CLAUDE.md's
/// native-lifecycle testing rule).
#[derive(Debug, Clone, Copy)]
pub struct StartupDemandInputs {
    /// The window is being torn down; demand is about to stop entirely.
    pub closing: bool,
    /// The panel reported a real, non-empty content frame.
    pub geometry_visible: bool,
    /// AppKit's raw `isVisible`/`occlusionState` answer for the panel.
    pub appkit_reports_occluded: bool,
    /// A decoded frame has landed, so the panel is a real window rather than
    /// the pre-first-frame placeholder.
    pub first_frame_seen: bool,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

/// Choose the subscription dimensions to request for one receiver window.
///
/// #299/#590: a receiver panel is created HIDDEN and stays hidden until its
/// first decoded frame (`compositor.rs`'s placeholder), and
/// `platform::appkit::is_fully_occluded` reports every hidden window as fully
/// occluded (`!isVisible` is its first disjunct). Feeding that into the
/// occlusion path made the pre-first-frame window look like a viewer that did
/// not want pixels, so the second demand publication before the first frame
/// -- the 2s heartbeat, a geometry refresh, a DPI settle, a retired-window
/// reuse -- requested the lowest layer during exactly the interval the user is
/// waiting to see something. Occlusion is a statement about a window the user
/// can already see; it is not measurable before there is anything to see.
pub fn startup_demand_decision(window_id: u32, inputs: StartupDemandInputs) -> (u32, u32) {
    if inputs.closing {
        return requested_dimensions(false, inputs.pixel_width, inputs.pixel_height);
    }
    // Only sample occlusion once a real frame has landed. Short-circuiting
    // here also keeps the hysteresis counter itself unpolluted, so the first
    // post-reveal sample starts from zero instead of arriving pre-tripped.
    let occluded =
        inputs.first_frame_seen && occlusion_is_debounced(window_id, inputs.appkit_reports_occluded);
    // "No geometry yet" is not "geometry unavailable" (#590). Before the first
    // frame the panel has no meaningful size, and `demand_pixel_dimensions`
    // has already substituted MAX_DEMAND_DIMENSION_PX for exactly that case --
    // don't then throw it away by calling the window invisible.
    let visible = inputs.geometry_visible || !inputs.first_frame_seen;
    requested_dimensions(visible && !occluded, inputs.pixel_width, inputs.pixel_height)
}

fn publish_message(app: &AppHandle, message: ViewerDemandMessage) {
    let Some((room_connection, _)) = app
        .try_state::<SessionState>()
        .and_then(|state| state.control_channel_snapshot())
    else {
        return;
    };
    let room = room_connection.room();
    tauri::async_runtime::spawn(async move {
        let Ok(payload) = serde_json::to_vec(&message) else {
            return;
        };
        let packet = livekit::DataPacket {
            payload,
            topic: Some(TOPIC.to_string()),
            reliable: true,
            destination_identities: vec![livekit::prelude::ParticipantIdentity(
                message.target_user_id.clone(),
            )],
        };
        if let Err(e) = room.local_participant().publish_data(packet).await {
            log::debug!("viewer-demand: publish_data failed: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct ContractFixture {
        topics: ContractTopics,
        #[serde(rename = "viewerDemandFields")]
        viewer_demand_fields: Vec<String>,
    }

    #[derive(serde::Deserialize)]
    struct ContractTopics {
        #[serde(rename = "viewerDemand")]
        viewer_demand: String,
    }

    fn contract_fixture() -> ContractFixture {
        serde_json::from_str(include_str!("../../../../contracts/petal-contracts.json")).unwrap()
    }

    #[test]
    fn topic_is_pinned() {
        assert_eq!(TOPIC, contract_fixture().topics.viewer_demand);
        assert!(crate::session::VIEWER_DEMAND_STALE_AFTER > HEARTBEAT_INTERVAL);
    }

    #[test]
    fn message_json_fields_match_contract() {
        let message = ViewerDemandMessage {
            v: WIRE_VERSION,
            kind: ViewerDemandKind::Heartbeat,
            target_user_id: "native-1".to_string(),
            viewer_id: "viewer-1".to_string(),
            window_id: 42,
            seq: 9,
            visible: true,
            width: 1280,
            height: 720,
            scale: 2.0,
            pixel_width: 2560,
            pixel_height: 1440,
            needs_republish: false,
        };
        let value = serde_json::to_value(&message).unwrap();
        let mut keys = value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        assert_eq!(keys, contract_fixture().viewer_demand_fields);
    }

    #[test]
    fn legacy_message_derives_pixels_at_one_x() {
        let message: ViewerDemandMessage = serde_json::from_value(serde_json::json!({
            "kind": "heartbeat",
            "targetUserId": "native-1",
            "viewerId": "legacy-viewer",
            "windowId": 42,
            "seq": 9,
            "visible": true,
            "width": 1280,
            "height": 720
        }))
        .unwrap();
        assert_eq!(message.v, 1);
        assert_eq!(normalized_geometry(&message), (1.0, 1280, 720));
    }

    #[test]
    fn legacy_message_ignores_unversioned_scale_and_pixel_extensions() {
        let message: ViewerDemandMessage = serde_json::from_value(serde_json::json!({
            "kind": "heartbeat",
            "targetUserId": "native-1",
            "viewerId": "legacy-viewer",
            "windowId": 42,
            "seq": 10,
            "visible": true,
            "width": 1280,
            "height": 720,
            "scale": 2.0,
            "pixelWidth": 2560,
            "pixelHeight": 1440
        }))
        .unwrap();
        assert_eq!(message.v, 1);
        assert_eq!(normalized_geometry(&message), (1.0, 1280, 720));
    }

    #[test]
    fn initial_demand_does_not_advertise_placeholder_pixels() {
        assert_eq!(
            demand_pixel_dimensions(640, 400, 1.0, false),
            (MAX_DEMAND_DIMENSION_PX, MAX_DEMAND_DIMENSION_PX)
        );
        assert_eq!(demand_pixel_dimensions(640, 400, 1.0, true), (640, 400));
    }

    #[test]
    fn fully_occluded_receiver_requests_lowest_layer() {
        assert_eq!(requested_dimensions(false, 2560, 1440), (640, 360));
        assert_eq!(requested_dimensions(true, 2560, 1440), (2560, 1440));
    }

    #[test]
    fn occlusion_requires_two_samples_and_visible_restores_immediately() {
        let mut state = OcclusionHysteresis::default();
        assert!(!update_occlusion_hysteresis(&mut state, true));
        assert!(update_occlusion_hysteresis(&mut state, true));
        assert!(!update_occlusion_hysteresis(&mut state, false));
    }

    #[test]
    fn initial_demand_uses_receiver_scale_after_first_frame() {
        assert_eq!(demand_pixel_dimensions(640, 400, 2.0, true), (1280, 800));
    }

    /// The pre-first-frame panel, exactly as `demand_for_window` reports it:
    /// a real placeholder frame, AppKit calling the hidden window occluded,
    /// and `demand_pixel_dimensions` having already substituted MAX.
    fn hidden_placeholder() -> StartupDemandInputs {
        StartupDemandInputs {
            closing: false,
            geometry_visible: true,
            appkit_reports_occluded: true,
            first_frame_seen: false,
            pixel_width: MAX_DEMAND_DIMENSION_PX,
            pixel_height: MAX_DEMAND_DIMENSION_PX,
        }
    }

    fn revealed(occluded: bool) -> StartupDemandInputs {
        StartupDemandInputs {
            closing: false,
            geometry_visible: true,
            appkit_reports_occluded: occluded,
            first_frame_seen: true,
            pixel_width: 1920,
            pixel_height: 1080,
        }
    }

    /// Window ids are namespaced per test: `startup_demand_decision` shares
    /// one process-wide occlusion-hysteresis map, so reusing an id across
    /// tests would let one test's samples decide another's.
    #[test]
    fn repeated_pre_first_frame_demand_never_requests_the_lowest_layer() {
        // #299: the panel is created hidden and stays hidden until its first
        // decoded frame. Every demand publication in that window -- the Open,
        // a geometry/DPI settle, the 2s heartbeat -- used to feed AppKit's
        // "hidden means occluded" answer into the occlusion path, and the
        // SECOND one tripped the hysteresis and requested 640x360 (the q
        // layer, capped at 15fps) during exactly the interval the user is
        // waiting to see something.
        for publication in 0..5 {
            assert_eq!(
                startup_demand_decision(90_299, hidden_placeholder()),
                (MAX_DEMAND_DIMENSION_PX, MAX_DEMAND_DIMENSION_PX),
                "pre-first-frame demand #{publication} degraded the request"
            );
        }
    }

    #[test]
    fn pre_first_frame_samples_do_not_pre_trip_the_post_reveal_hysteresis() {
        // The counter must not accumulate while hidden, or the first genuinely
        // occluded sample after the reveal would downgrade immediately instead
        // of requiring its own two samples.
        for _ in 0..5 {
            startup_demand_decision(90_300, hidden_placeholder());
        }
        assert_eq!(startup_demand_decision(90_300, revealed(true)), (1920, 1080));
        assert_eq!(
            startup_demand_decision(90_300, revealed(true)),
            (LOWEST_LAYER_WIDTH, LOWEST_LAYER_HEIGHT)
        );
    }

    #[test]
    fn genuine_occlusion_after_first_frame_still_downgrades() {
        // The occlusion saving is not disabled, only deferred until it can
        // mean something: a window the user can actually see.
        assert_eq!(startup_demand_decision(90_301, revealed(true)), (1920, 1080));
        assert_eq!(
            startup_demand_decision(90_301, revealed(true)),
            (LOWEST_LAYER_WIDTH, LOWEST_LAYER_HEIGHT)
        );
        // ...and a visible sample restores full demand immediately.
        assert_eq!(
            startup_demand_decision(90_301, revealed(false)),
            (1920, 1080)
        );
    }

    #[test]
    fn missing_geometry_before_the_first_frame_is_not_treated_as_unavailable() {
        // #590 part 1: `content_frame_and_scale_for_window` returning None
        // pre-first-frame is "not measured yet", not "the viewer is gone".
        let mut inputs = hidden_placeholder();
        inputs.geometry_visible = false;
        assert_eq!(
            startup_demand_decision(90_302, inputs),
            (MAX_DEMAND_DIMENSION_PX, MAX_DEMAND_DIMENSION_PX)
        );
    }

    #[test]
    fn missing_geometry_after_the_first_frame_still_requests_the_lowest_layer() {
        // Once a frame has landed, a window with no resolvable geometry really
        // is gone/unavailable, and should not keep holding a large layer.
        let mut inputs = revealed(false);
        inputs.geometry_visible = false;
        assert_eq!(
            startup_demand_decision(90_303, inputs),
            (LOWEST_LAYER_WIDTH, LOWEST_LAYER_HEIGHT)
        );
    }

    #[test]
    fn closing_demand_is_unchanged() {
        let mut inputs = revealed(false);
        inputs.closing = true;
        assert_eq!(
            startup_demand_decision(90_304, inputs),
            (LOWEST_LAYER_WIDTH, LOWEST_LAYER_HEIGHT)
        );
    }
}
