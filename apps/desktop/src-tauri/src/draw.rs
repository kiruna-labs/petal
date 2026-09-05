use std::sync::Arc;

use livekit::Room;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::session::{RoomGeneration, SessionState};
use crate::transport::publisher::RoomConnection;

pub const TOPIC: &str = "petal.draw";
const DRAW_UPDATE_EVENT: &str = "draw-update";
const VERSION: u8 = 1;
const MAX_POINTS_PER_MESSAGE: usize = 128;
const MAX_TEXT_CHARS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DrawMessageType {
    Begin,
    Points,
    End,
    Clear,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DrawPoint {
    x: f64,
    y: f64,
}

impl DrawPoint {
    fn clamped(self) -> Self {
        Self {
            x: self.x.clamp(0.0, 1.0),
            y: self.y.clamp(0.0, 1.0),
        }
    }

    fn is_normalized(self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && (0.0..=1.0).contains(&self.x)
            && (0.0..=1.0).contains(&self.y)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DrawDraft {
    #[serde(rename = "type")]
    message_type: DrawMessageType,
    window_id: u32,
    owner_identity: String,
    stroke_id: Option<String>,
    seq: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    points: Vec<DrawPoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DrawWireMessage {
    v: u8,
    #[serde(rename = "type")]
    message_type: DrawMessageType,
    window_id: u32,
    owner_identity: String,
    stroke_id: Option<String>,
    seq: u64,
    // Always present (even as `[]` for end/clear), never omitted -- the
    // shared contract fixture (contracts/petal-contracts.json) pins a
    // uniform envelope shape across all message types so both sides
    // can parse without a per-variant field-presence check.
    #[serde(default)]
    points: Vec<DrawPoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DrawUpdate {
    #[serde(rename = "type")]
    message_type: DrawMessageType,
    window_id: u32,
    owner_identity: String,
    drawer_identity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    drawer_display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    drawer_palette_index: Option<u8>,
    stroke_id: Option<String>,
    seq: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    points: Vec<DrawPoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

#[tauri::command]
pub async fn draw_send(app: AppHandle, draft: DrawDraft) -> Result<(), String> {
    let (room_connection, drawer_identity) = draw_channel(&app)?;
    let target_owner = if can_draw_on_own_window(
        &draft.owner_identity,
        &drawer_identity,
        app.try_state::<SessionState>()
            .is_some_and(|state| state.is_share_active(draft.window_id)),
    ) {
        draft.owner_identity.clone()
    } else {
        remote_owner_identity(draft.window_id, &draft.owner_identity)
            .ok_or_else(|| format!("remote window {} is not open", draft.window_id))?
    };
    draft.validate()?;
    let message = draft.into_message(target_owner);
    let drawer_palette_index = room_connection.identity_palette_index();
    publish_message(room_connection, message.clone()).await?;
    deliver_update(
        &app,
        update_for_authenticated_sender(message, drawer_identity, None, drawer_palette_index),
    );
    Ok(())
}

fn remote_owner_identity(window_id: u32, owner_identity: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        return crate::compositor::owner_identity_for_window(window_id, Some(owner_identity));
    }

    #[cfg(target_os = "windows")]
    {
        let key = (owner_identity.to_string(), window_id);
        return crate::windows_compositor::window_open_for(&key)
            .then(|| owner_identity.to_string());
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    None
}

fn can_draw_on_own_window(
    owner_identity: &str,
    drawer_identity: &str,
    local_share_active: bool,
) -> bool {
    local_share_active && owner_identity == drawer_identity
}

fn validate_text_annotation(text: Option<&str>) -> Result<(), &'static str> {
    let Some(text) = text else {
        return Err("draw text annotation requires text");
    };
    if text.trim().is_empty() {
        return Err("draw text annotation must not be blank");
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err("draw text annotation is too long");
    }
    if text
        .chars()
        .any(|character| matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
    {
        return Err("draw text annotation must be single-line");
    }
    Ok(())
}

fn draw_channel(app: &AppHandle) -> Result<(Arc<RoomConnection>, String), String> {
    let state = app
        .try_state::<SessionState>()
        .ok_or_else(|| "session state is not available".to_string())?;
    state
        .inner()
        .control_channel_snapshot()
        .ok_or_else(|| "join a room before drawing on a shared window".to_string())
}

impl DrawDraft {
    fn validate(&self) -> Result<(), String> {
        if self.message_type == DrawMessageType::Clear {
            if self.stroke_id.is_some() || !self.points.is_empty() || self.text.is_some() {
                return Err("draw clear must not include a stroke id, points, or text".to_string());
            }
            return Ok(());
        }
        if self.points.len() > MAX_POINTS_PER_MESSAGE {
            return Err("draw payload has too many points".to_string());
        }
        if self.stroke_id.as_deref().unwrap_or("").trim().is_empty() {
            return Err("draw stroke id is required".to_string());
        }
        if self.message_type == DrawMessageType::Text {
            if self.points.len() != 1 {
                return Err("draw text annotation requires one anchor point".to_string());
            }
            validate_text_annotation(self.text.as_deref()).map_err(str::to_string)?;
            return Ok(());
        }
        if self.text.is_some() {
            return Err("draw text is only valid for text annotations".to_string());
        }
        if self.message_type != DrawMessageType::End && self.points.is_empty() {
            return Err("draw begin/points messages require points".to_string());
        }
        Ok(())
    }

    fn into_message(self, owner_identity: String) -> DrawWireMessage {
        DrawWireMessage {
            v: VERSION,
            message_type: self.message_type,
            window_id: self.window_id,
            owner_identity,
            stroke_id: self.stroke_id,
            seq: self.seq,
            points: self
                .points
                .into_iter()
                .take(MAX_POINTS_PER_MESSAGE)
                .map(DrawPoint::clamped)
                .collect(),
            text: self.text,
        }
    }
}

impl DrawWireMessage {
    fn validate(&self) -> Result<(), &'static str> {
        if self.v != VERSION {
            return Err("unsupported draw payload version");
        }
        if self.owner_identity.trim().is_empty() {
            return Err("draw owner identity is required");
        }
        if self.points.len() > MAX_POINTS_PER_MESSAGE {
            return Err("draw payload has too many points");
        }
        if self.points.iter().any(|point| !point.is_normalized()) {
            return Err("draw payload points must be normalized");
        }
        if self.message_type == DrawMessageType::Clear {
            if self.stroke_id.is_some() || !self.points.is_empty() || self.text.is_some() {
                return Err("draw clear must not include a stroke id, points, or text");
            }
            return Ok(());
        }
        if self.stroke_id.as_deref().unwrap_or("").trim().is_empty() {
            return Err("draw stroke id is required");
        }
        if self.message_type == DrawMessageType::Text {
            if self.points.len() != 1 {
                return Err("draw text annotation requires one anchor point");
            }
            validate_text_annotation(self.text.as_deref())?;
            return Ok(());
        }
        if self.text.is_some() {
            return Err("draw text is only valid for text annotations");
        }
        if self.message_type != DrawMessageType::End && self.points.is_empty() {
            return Err("draw begin/points messages require points");
        }
        Ok(())
    }
}

async fn publish_message(
    room_connection: Arc<RoomConnection>,
    message: DrawWireMessage,
) -> Result<(), String> {
    let payload = serde_json::to_vec(&message).map_err(|e| e.to_string())?;
    let packet = livekit::DataPacket {
        payload,
        topic: Some(TOPIC.to_string()),
        reliable: true,
        destination_identities: Vec::new(),
    };
    room_connection
        .room()
        .local_participant()
        .publish_data(packet)
        .await
        .map_err(|e| e.to_string())
}

pub fn start_receiver_for_room(app: &AppHandle, room: Arc<Room>, generation: RoomGeneration) {
    let mut events = room.subscribe();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                log::debug!("draw: receiver exiting for stale room generation");
                break;
            }
            let livekit::RoomEvent::DataReceived {
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
            let Ok(message) = serde_json::from_slice::<DrawWireMessage>(&payload) else {
                log::warn!("draw: ignored malformed draw payload");
                continue;
            };
            if let Err(reason) = message.validate() {
                log::warn!("draw: ignored invalid draw payload: {reason}");
                continue;
            }
            let Some(sender_identity) = participant.as_ref().map(|p| p.identity().to_string())
            else {
                log::warn!("draw: ignored update without authenticated sender identity");
                continue;
            };
            let display_name = participant.as_ref().and_then(|p| {
                let name = p.name();
                let trimmed = name.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            });
            let palette_index = participant.as_ref().and_then(|p| {
                crate::transport::publisher::identity_palette_index_from_metadata(&p.metadata())
            });
            let update = update_for_authenticated_sender(
                message,
                sender_identity,
                display_name,
                palette_index,
            );
            deliver_update(&app, update);
        }
    });
}

fn update_for_authenticated_sender(
    message: DrawWireMessage,
    sender_identity: String,
    display_name: Option<String>,
    palette_index: Option<u8>,
) -> DrawUpdate {
    DrawUpdate {
        message_type: message.message_type,
        window_id: message.window_id,
        owner_identity: message.owner_identity,
        drawer_identity: sender_identity,
        drawer_display_name: display_name,
        drawer_palette_index: palette_index,
        stroke_id: message.stroke_id,
        seq: message.seq,
        points: message
            .points
            .into_iter()
            .take(MAX_POINTS_PER_MESSAGE)
            .collect(),
        text: message.text,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DrawDeliveryTarget {
    SharerOverlay(String),
    RemotePointerOverlay(String),
}

fn select_delivery_target(
    owner_identity: &str,
    local_identity: Option<&str>,
    sharer_overlay_label: Option<String>,
    remote_overlay_label: Option<String>,
) -> Option<DrawDeliveryTarget> {
    if local_identity == Some(owner_identity) {
        sharer_overlay_label.map(DrawDeliveryTarget::SharerOverlay)
    } else {
        remote_overlay_label.map(DrawDeliveryTarget::RemotePointerOverlay)
    }
}

fn should_log_stroke_delivery(message_type: DrawMessageType, target: &DrawDeliveryTarget) -> bool {
    matches!(target, DrawDeliveryTarget::SharerOverlay(_))
        && matches!(message_type, DrawMessageType::Begin | DrawMessageType::End)
}

fn is_camera_window_id(window_id: u32) -> bool {
    window_id & 0x8000_0000 != 0
}

fn format_stroke_delivery_log(update: &DrawUpdate, overlay_label: &str) -> String {
    let stroke_id = update.stroke_id.as_deref().unwrap_or("(none)");
    format!(
        "draw: delivered {:?} stroke '{}' from '{}' to own shared window {} via sharer overlay '{}'",
        update.message_type, stroke_id, update.drawer_identity, update.window_id, overlay_label
    )
}

fn local_identity(app: &AppHandle) -> Option<String> {
    app.try_state::<SessionState>().and_then(|state| {
        state
            .control_channel_snapshot()
            .map(|(_, identity)| identity)
    })
}

fn remote_pointer_overlay_label(window_id: u32, owner_identity: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        return crate::compositor::pointer_label_for_remote_window(window_id, owner_identity);
    }

    #[cfg(target_os = "windows")]
    {
        return crate::windows_compositor::pointer_overlay_labels_for(owner_identity, window_id)
            .into_iter()
            .next();
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    None
}

fn deliver_update(app: &AppHandle, update: DrawUpdate) {
    if is_camera_window_id(update.window_id) {
        log::debug!(
            "draw: received camera-surface draw payload for owner '{}' synthetic window {}",
            update.owner_identity,
            update.window_id
        );
        if let Err(e) = app.emit(DRAW_UPDATE_EVENT, update) {
            log::warn!("draw: failed to emit camera-surface draw update to meeting gallery: {e}");
        }
        return;
    }
    let local_identity = local_identity(app);
    let is_local_owner = local_identity.as_deref() == Some(update.owner_identity.as_str());
    let sharer_overlay_label = is_local_owner
        .then(|| {
            crate::share_overlay::overlay_label_for_window(update.window_id, &update.owner_identity)
        })
        .flatten();
    let remote_overlay_label = (!is_local_owner)
        .then(|| remote_pointer_overlay_label(update.window_id, &update.owner_identity))
        .flatten();
    let Some(target) = select_delivery_target(
        &update.owner_identity,
        local_identity.as_deref(),
        sharer_overlay_label,
        remote_overlay_label,
    ) else {
        if is_local_owner {
            log::debug!(
                "draw: no sharer overlay for local owner '{}' window {}",
                update.owner_identity,
                update.window_id
            );
        } else {
            log::debug!(
                "draw: no pointer overlay for owner '{}' window {}",
                update.owner_identity,
                update.window_id
            );
        }
        return;
    };
    let log_stroke_delivery = should_log_stroke_delivery(update.message_type, &target);
    let (label, surface) = match target {
        DrawDeliveryTarget::SharerOverlay(label) => (label, "sharer overlay"),
        DrawDeliveryTarget::RemotePointerOverlay(label) => (label, "remote pointer overlay"),
    };
    let Some(overlay) = app.get_webview_window(&label) else {
        return;
    };
    let Ok(json) = serde_json::to_string(&update) else {
        return;
    };
    if let Err(e) = overlay.eval(format!("window.__petalDraw && window.__petalDraw({json})")) {
        log::warn!(
            "draw: failed to eval update for window {} overlay '{}': {e}",
            update.window_id,
            overlay.label()
        );
    } else {
        log::trace!(
            "draw: delivered update for owner '{}' window {} to {surface} '{}'",
            update.owner_identity,
            update.window_id,
            overlay.label()
        );
        if log_stroke_delivery {
            let message = format_stroke_delivery_log(&update, overlay.label());
            log::info!("{message}");
            // Also journal this (not just petal.log) so the test-cockpit's
            // DRAW-N assertion -- which polls DiagnosticsState::journal(),
            // not the file log -- can actually observe stroke delivery.
            // Prior to this, draw.rs never called journal_append at all,
            // making the DRAW-N scenario's native-side check structurally
            // unreachable no matter how the drawing itself behaved.
            if let Some(diagnostics) = app.try_state::<crate::diagnostics::DiagnosticsState>() {
                diagnostics.journal_append(app, "draw", message);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct ContractFixture {
        topics: ContractTopics,
        #[serde(rename = "drawMessages")]
        draw_messages: Vec<DrawContractVector>,
    }

    #[derive(serde::Deserialize)]
    struct ContractTopics {
        draw: String,
    }

    #[derive(serde::Deserialize)]
    struct DrawContractVector {
        name: String,
        reliable: bool,
        message: DrawWireMessage,
        fields: Vec<String>,
        #[serde(rename = "pointFields")]
        point_fields: Vec<String>,
    }

    fn contract_fixture() -> ContractFixture {
        serde_json::from_str(include_str!("../../../../contracts/petal-contracts.json")).unwrap()
    }

    #[test]
    fn draw_update_uses_authenticated_sender_identity() {
        let update = update_for_authenticated_sender(
            DrawWireMessage {
                v: VERSION,
                message_type: DrawMessageType::Begin,
                window_id: 7,
                owner_identity: "owner".into(),
                stroke_id: Some("stroke-1".into()),
                seq: 1,
                points: vec![DrawPoint { x: 0.25, y: 0.75 }],
                text: None,
            },
            "real-sender".into(),
            Some("Real Sender".into()),
            Some(4),
        );

        assert_eq!(update.drawer_identity, "real-sender");
        assert_eq!(update.drawer_display_name.as_deref(), Some("Real Sender"));
        assert_eq!(update.drawer_palette_index, Some(4));
        assert_eq!(update.points, vec![DrawPoint { x: 0.25, y: 0.75 }]);
    }

    #[test]
    fn draw_delivery_routes_local_owner_updates_to_sharer_overlay() {
        let target = select_delivery_target(
            "owner-a",
            Some("owner-a"),
            Some("share_overlay_7".to_string()),
            Some("remote-window-pointer-wrong".to_string()),
        );

        assert_eq!(
            target,
            Some(DrawDeliveryTarget::SharerOverlay(
                "share_overlay_7".to_string()
            ))
        );
    }

    #[test]
    fn draw_delivery_routes_remote_owner_updates_to_remote_pointer_overlay() {
        let target = select_delivery_target(
            "owner-b",
            Some("owner-a"),
            Some("share_overlay_wrong".to_string()),
            Some("remote-window-pointer-1".to_string()),
        );

        assert_eq!(
            target,
            Some(DrawDeliveryTarget::RemotePointerOverlay(
                "remote-window-pointer-1".to_string()
            ))
        );
    }

    #[test]
    fn draw_delivery_does_not_fall_back_to_remote_overlay_for_local_owner() {
        let target = select_delivery_target(
            "owner-a",
            Some("owner-a"),
            None,
            Some("remote-window-pointer-wrong".to_string()),
        );

        assert_eq!(target, None);
    }

    #[test]
    fn draw_delivery_logs_only_own_window_begin_and_end() {
        let target = DrawDeliveryTarget::SharerOverlay("share_overlay_7".to_string());
        assert!(should_log_stroke_delivery(DrawMessageType::Begin, &target));
        assert!(should_log_stroke_delivery(DrawMessageType::End, &target));
        assert!(!should_log_stroke_delivery(
            DrawMessageType::Points,
            &target
        ));
        assert!(!should_log_stroke_delivery(DrawMessageType::Clear, &target));
        assert!(!should_log_stroke_delivery(
            DrawMessageType::Begin,
            &DrawDeliveryTarget::RemotePointerOverlay("pointer_7".to_string())
        ));
    }

    #[test]
    fn draw_stroke_delivery_log_includes_sender_stroke_and_overlay() {
        let update = DrawUpdate {
            message_type: DrawMessageType::Begin,
            window_id: 7,
            owner_identity: "owner-a".into(),
            drawer_identity: "drawer-b".into(),
            drawer_display_name: Some("Drawer B".into()),
            drawer_palette_index: None,
            stroke_id: Some("stroke-42".into()),
            seq: 9,
            points: vec![DrawPoint { x: 0.25, y: 0.75 }],
            text: None,
        };

        assert_eq!(
            format_stroke_delivery_log(&update, "share_overlay_7"),
            "draw: delivered Begin stroke 'stroke-42' from 'drawer-b' to own shared window 7 via sharer overlay 'share_overlay_7'"
        );
    }

    #[test]
    fn draw_payload_serializes_camel_case_type_and_owner() {
        let message = DrawDraft {
            message_type: DrawMessageType::Points,
            window_id: 12,
            owner_identity: "owner-1".into(),
            stroke_id: Some("stroke-2".into()),
            seq: 3,
            points: vec![DrawPoint { x: 0.25, y: 0.75 }],
            text: None,
        }
        .into_message("owner-1".into());

        let json = serde_json::to_value(message).unwrap();
        assert_eq!(json["v"], VERSION);
        assert_eq!(json["type"], "points");
        assert_eq!(json["windowId"], 12);
        assert_eq!(json["ownerIdentity"], "owner-1");
        assert_eq!(json["strokeId"], "stroke-2");
        assert_eq!(json["points"][0]["x"], 0.25);
    }

    #[test]
    fn draw_text_annotation_serializes_and_rejects_multiline_input() {
        let draft = DrawDraft {
            message_type: DrawMessageType::Text,
            window_id: 12,
            owner_identity: "owner-1".into(),
            stroke_id: Some("text-1".into()),
            seq: 4,
            points: vec![DrawPoint { x: 0.25, y: 0.75 }],
            text: Some("Hello Petal".into()),
        };
        let message = draft.into_message("owner-1".into());
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "Hello Petal");
        assert!(message.validate().is_ok());

        let mut multiline = message;
        multiline.text = Some("one\ntwo".into());
        assert_eq!(multiline.validate(), Err("draw text annotation must be single-line"));
    }

    #[test]
    fn draw_draft_clamps_outgoing_points_before_publish() {
        let message = DrawDraft {
            message_type: DrawMessageType::Points,
            window_id: 12,
            owner_identity: "owner-1".into(),
            stroke_id: Some("stroke-2".into()),
            seq: 3,
            points: vec![DrawPoint { x: -0.25, y: 1.25 }],
            text: None,
        }
        .into_message("owner-1".into());

        assert_eq!(message.points, vec![DrawPoint { x: 0.0, y: 1.0 }]);
        assert!(message.validate().is_ok());
    }

    #[test]
    fn draw_wire_shape_matches_shared_contract_fixture() {
        let fixture = contract_fixture();
        assert_eq!(TOPIC, fixture.topics.draw);
        assert_eq!(
            fixture
                .draw_messages
                .iter()
                .map(|vector| vector.name.as_str())
                .collect::<Vec<_>>(),
            vec!["begin", "points", "end", "text", "clear", "camera-begin"]
        );

        for vector in fixture.draw_messages {
            assert!(vector.reliable, "{} must be reliable", vector.name);
            vector
                .message
                .validate()
                .unwrap_or_else(|err| panic!("{} fixture invalid: {err}", vector.name));
            let value = serde_json::to_value(&vector.message).unwrap();
            let mut fields = value
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            fields.sort();
            assert_eq!(fields, vector.fields, "{}", vector.name);
            for point in value["points"].as_array().unwrap() {
                let mut point_fields = point
                    .as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                point_fields.sort();
                assert_eq!(point_fields, vector.point_fields, "{}", vector.name);
            }
        }
    }

    #[test]
    fn draw_wire_accepts_camera_synthetic_window_ids() {
        let message = DrawWireMessage {
            v: VERSION,
            message_type: DrawMessageType::Begin,
            window_id: 0x8000_1234,
            owner_identity: "camera-owner".into(),
            stroke_id: Some("camera-stroke".into()),
            seq: 1,
            points: vec![DrawPoint { x: 0.25, y: 0.75 }],
            text: None,
        };

        assert!(is_camera_window_id(message.window_id));
        assert!(message.validate().is_ok());
        let update = update_for_authenticated_sender(message, "drawer".into(), None, None);
        assert_eq!(update.window_id, 0x8000_1234);
        assert_eq!(update.owner_identity, "camera-owner");
    }

    #[test]
    fn camera_surface_draws_use_meeting_gallery_event() {
        assert_eq!(DRAW_UPDATE_EVENT, "draw-update");
    }

    #[test]
    fn draw_wire_validation_rejects_wrong_version_and_bad_clear() {
        let mut message = DrawWireMessage {
            v: 2,
            message_type: DrawMessageType::Begin,
            window_id: 12,
            owner_identity: "owner-1".into(),
            stroke_id: Some("stroke-2".into()),
            seq: 3,
            points: vec![DrawPoint { x: 0.25, y: 0.75 }],
            text: None,
        };

        assert!(message.validate().is_err());
        message.v = VERSION;
        assert!(message.validate().is_ok());

        message.message_type = DrawMessageType::Clear;
        assert!(message.validate().is_err());
        message.stroke_id = None;
        message.points.clear();
        assert!(message.validate().is_ok());
    }

    #[test]
    fn draw_wire_validation_rejects_non_normalized_points() {
        let mut message = DrawWireMessage {
            v: VERSION,
            message_type: DrawMessageType::Points,
            window_id: 12,
            owner_identity: "owner-1".into(),
            stroke_id: Some("stroke-2".into()),
            seq: 3,
            points: vec![DrawPoint { x: 1.1, y: 0.75 }],
            text: None,
        };

        assert_eq!(
            message.validate(),
            Err("draw payload points must be normalized")
        );
        message.points = vec![DrawPoint { x: 0.25, y: 0.75 }];
        assert!(message.validate().is_ok());
    }

    #[test]
    fn draw_draft_validation_rejects_oversized_point_batches() {
        let draft = DrawDraft {
            message_type: DrawMessageType::Points,
            window_id: 12,
            owner_identity: "owner-1".into(),
            stroke_id: Some("stroke-2".into()),
            seq: 3,
            points: vec![DrawPoint { x: 0.25, y: 0.75 }; MAX_POINTS_PER_MESSAGE + 1],
            text: None,
        };

        assert_eq!(
            draft.validate(),
            Err("draw payload has too many points".to_string())
        );
    }

    #[test]
    fn draw_send_allows_authenticated_drawer_to_target_active_own_window() {
        assert!(can_draw_on_own_window("owner-a", "owner-a", true));
        assert!(!can_draw_on_own_window("owner-a", "owner-a", false));
        assert!(!can_draw_on_own_window("owner-a", "drawer-b", true));
    }

    #[test]
    fn local_echo_preserves_authenticated_drawer_palette_index() {
        let message = DrawWireMessage {
            v: VERSION,
            message_type: DrawMessageType::Begin,
            window_id: 7,
            owner_identity: "owner-a".into(),
            stroke_id: Some("stroke-1".into()),
            seq: 1,
            points: vec![DrawPoint { x: 0.25, y: 0.75 }],
            text: None,
        };

        let update = update_for_authenticated_sender(message, "drawer-a".into(), None, Some(3));

        assert_eq!(update.drawer_palette_index, Some(3));
    }

    #[test]
    fn sharer_draw_stroke_keeps_contract_envelope_and_renders_as_owner_drawer() {
        let fixture = contract_fixture();
        let begin = fixture
            .draw_messages
            .iter()
            .find(|vector| vector.name == "begin")
            .unwrap();
        let draft = DrawDraft {
            message_type: DrawMessageType::Begin,
            window_id: begin.message.window_id,
            owner_identity: "owner-a".into(),
            stroke_id: Some("sharer-stroke".into()),
            seq: begin.message.seq,
            points: begin.message.points.clone(),
            text: None,
        };
        let message = draft.into_message("owner-a".into());
        let json = serde_json::to_value(&message).unwrap();
        let mut fields = json
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        fields.sort();

        assert_eq!(fields, begin.fields);
        assert_eq!(message.validate(), Ok(()));
        let parsed: DrawWireMessage = serde_json::from_value(json).unwrap();
        let update = update_for_authenticated_sender(parsed, "owner-a".into(), None, None);
        assert_eq!(update.owner_identity, "owner-a");
        assert_eq!(update.drawer_identity, "owner-a");
        assert_eq!(update.points, begin.message.points);
    }
}
