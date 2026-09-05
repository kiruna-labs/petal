/// Payload for the shared `resilience-event` Tauri event.
///
/// `using_default` is present only when a native device watcher knows an
/// automatic fallback selected the system default. Older/macOS emitters omit
/// it, preserving their existing wire shape and frontend behavior.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ResilienceEvent {
    Reconnecting,
    Reconnected {
        message: String,
    },
    Disconnected {
        reason: String,
    },
    NetworkChanged,
    MicDeviceChanged {
        device_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        using_default: Option<bool>,
    },
    MicDeviceFailed {
        message: String,
    },
    SpeakerDeviceChanged {
        device_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        using_default: Option<bool>,
    },
    SpeakerDeviceFailed {
        message: String,
    },
    SharePublicationRepairRecovering {
        window_id: u32,
    },
    SharePublicationRepairCancelled {
        window_id: u32,
    },
    SharePublicationRepairRestored {
        window_id: u32,
    },
    SharePublicationRepairFailed {
        window_id: u32,
        message: String,
    },
    /// #713: still-failing mic/camera republish after reconnect repair --
    /// see `resilience::emit_mic_publication_repair_failed`/
    /// `emit_camera_publication_repair_failed`. At most one mic and one
    /// camera track exist per session, so unlike the window-share variants
    /// above these carry no id, just a message.
    MicPublicationRepairFailed {
        message: String,
    },
    CameraPublicationRepairFailed {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_device_events_serialize_for_shared_frontend() {
        let mic = serde_json::to_value(ResilienceEvent::MicDeviceChanged {
            device_name: "USB Mic".into(),
            using_default: None,
        })
        .unwrap();
        assert_eq!(mic["kind"], "micDeviceChanged");
        assert_eq!(mic["deviceName"], "USB Mic");
        assert!(mic.get("usingDefault").is_none());

        let speaker = serde_json::to_value(ResilienceEvent::SpeakerDeviceChanged {
            device_name: "USB Speakers".into(),
            using_default: Some(true),
        })
        .unwrap();
        assert_eq!(speaker["kind"], "speakerDeviceChanged");
        assert_eq!(speaker["deviceName"], "USB Speakers");
        assert_eq!(speaker["usingDefault"], true);

        let failed = serde_json::to_value(ResilienceEvent::SpeakerDeviceFailed {
            message: "Speaker disconnected — check output device".into(),
        })
        .unwrap();
        assert_eq!(failed["kind"], "speakerDeviceFailed");
        assert_eq!(
            failed["message"],
            "Speaker disconnected — check output device"
        );
    }

    /// #713: mic/camera reconnect publication-repair failure notices must
    /// serialize with the tagged `kind` shape `ToastHost.svelte` switches on,
    /// same contract discipline as `reconnected_event_serializes_with_
    /// expected_shape` above.
    #[test]
    fn local_track_publication_repair_failed_events_serialize_with_expected_shape() {
        let mic = serde_json::to_value(ResilienceEvent::MicPublicationRepairFailed {
            message: "Reconnect could not restore your microphone".into(),
        })
        .unwrap();
        assert_eq!(mic["kind"], "micPublicationRepairFailed");
        assert_eq!(mic["message"], "Reconnect could not restore your microphone");

        let camera = serde_json::to_value(ResilienceEvent::CameraPublicationRepairFailed {
            message: "Reconnect could not restore your camera".into(),
        })
        .unwrap();
        assert_eq!(camera["kind"], "cameraPublicationRepairFailed");
        assert_eq!(camera["message"], "Reconnect could not restore your camera");
    }
}
