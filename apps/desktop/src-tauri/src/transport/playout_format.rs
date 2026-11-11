//! Playout output-format watch (#544).
//!
//! WebRTC's macOS ADM (`modules/audio_device/mac/audio_device_mac.cc`) builds
//! its render converter (48 kHz -> device rate) in `InitPlayout`. Its own
//! `kAudioDevicePropertyStreamFormat` listener is meant to rebuild that
//! converter when the device's format changes mid-call, but
//! `HandleStreamFormatChange` rejects any new format whose channel count differs
//! from the *recording* channel count (`_recChannels`, mono) -- for both scopes
//! -- and returns before touching the output converter. A stereo output that
//! changes rate therefore keeps converting 48 kHz -> the OLD rate while the
//! device consumes at the new one: playback runs slow and pitched down until a
//! manual output switch re-runs `InitPlayout`. Measured on Mac Studio Speakers
//! at 48 -> 44.1 kHz: the ADM logs `Changing channels not supported
//! (mChannelsPerFrame = 2)` and playout pulls drop from 1000 to 919 per 10 s
//! (exactly 44100/48000). This module notices the change and re-applies the
//! current playout device through the same switch path.
//!
//! Only the decision is platform-neutral; reading the format is macOS-only
//! (WASAPI shared mode resamples, so Windows has no equivalent defect).

/// What the current playout device reports. `device` is the CoreAudio
/// `AudioDeviceID`, so a default-device change reads as a device change (the
/// ADM re-inits playout itself on that path) rather than a format change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputFormat {
    pub device: u32,
    pub sample_rate_hz: u32,
    pub channels: u32,
    pub transport: DeviceClass,
}

impl OutputFormat {
    fn same_format(&self, other: &OutputFormat) -> bool {
        self.sample_rate_hz == other.sample_rate_hz && self.channels == other.channels
    }
}

/// Device-class buckets for logs -- never the device name or UID (AirPods
/// names carry the owner's name).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceClass {
    BuiltIn,
    Bluetooth,
    BluetoothLe,
    Usb,
    Hdmi,
    DisplayPort,
    AirPlay,
    Thunderbolt,
    Virtual,
    Aggregate,
    Other,
}

impl DeviceClass {
    pub(crate) fn from_transport_fourcc(code: u32) -> Self {
        match &code.to_be_bytes() {
            b"bltn" => Self::BuiltIn,
            b"blue" => Self::Bluetooth,
            b"blea" => Self::BluetoothLe,
            b"usb " => Self::Usb,
            b"hdmi" => Self::Hdmi,
            b"dprt" => Self::DisplayPort,
            b"airp" => Self::AirPlay,
            b"thun" => Self::Thunderbolt,
            b"virt" => Self::Virtual,
            b"grup" => Self::Aggregate,
            _ => Self::Other,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::Bluetooth => "bluetooth",
            Self::BluetoothLe => "bluetooth-le",
            Self::Usb => "usb",
            Self::Hdmi => "hdmi",
            Self::DisplayPort => "displayport",
            Self::AirPlay => "airplay",
            Self::Thunderbolt => "thunderbolt",
            Self::Virtual => "virtual",
            Self::Aggregate => "aggregate",
            Self::Other => "other",
        }
    }
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "class={} rate={}Hz channels={}",
            self.transport.label(),
            self.sample_rate_hz,
            self.channels
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormatDecision {
    /// Nothing to do (same format, unreadable, or first baseline).
    Unchanged,
    /// The playout device itself changed -- re-baselined, no re-apply.
    DeviceChanged {
        from: OutputFormat,
        to: OutputFormat,
    },
    /// A different format was seen once; wait one more poll for it to settle
    /// (a Bluetooth profile switch lands as a burst of rate/channel changes).
    Settling {
        from: OutputFormat,
        to: OutputFormat,
    },
    /// The new format held across two polls: re-apply the playout device.
    Reapply {
        from: OutputFormat,
        to: OutputFormat,
    },
}

/// The debounce/re-apply decision, kept pure so it is testable without a
/// sound card. `applied` is the format playout was last (re)initialized
/// against; `pending` the unconfirmed change seen on the previous poll.
#[derive(Debug, Default)]
pub(crate) struct PlayoutFormatTracker {
    applied: Option<OutputFormat>,
    pending: Option<OutputFormat>,
}

impl PlayoutFormatTracker {
    /// Record the format playout was just (re)initialized against.
    pub(crate) fn applied(&mut self, format: Option<OutputFormat>) {
        self.applied = format;
        self.pending = None;
    }

    pub(crate) fn observe(&mut self, observed: Option<OutputFormat>) -> FormatDecision {
        let Some(observed) = observed else {
            self.pending = None;
            return FormatDecision::Unchanged;
        };
        let Some(applied) = self.applied else {
            self.applied(Some(observed));
            return FormatDecision::Unchanged;
        };
        if observed.device != applied.device {
            self.applied(Some(observed));
            return FormatDecision::DeviceChanged {
                from: applied,
                to: observed,
            };
        }
        if observed.same_format(&applied) {
            self.pending = None;
            return FormatDecision::Unchanged;
        }
        if self
            .pending
            .is_some_and(|pending| pending.same_format(&observed))
        {
            self.pending = None;
            return FormatDecision::Reapply {
                from: applied,
                to: observed,
            };
        }
        self.pending = Some(observed);
        FormatDecision::Settling {
            from: applied,
            to: observed,
        }
    }
}

/// Read the output format of the device the ADM plays to. `adm_guid` is the
/// ADM's playout GUID: on macOS `"default"` (index 0, the system default
/// output) or the decimal CoreAudio `AudioDeviceID`.
#[cfg(target_os = "macos")]
pub(crate) fn read_output_format(adm_guid: &str) -> Option<OutputFormat> {
    mac::read(adm_guid)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn read_output_format(_adm_guid: &str) -> Option<OutputFormat> {
    None
}

#[cfg(target_os = "macos")]
mod mac {
    use super::{DeviceClass, OutputFormat};
    use std::ffi::c_void;

    const SYSTEM_OBJECT: u32 = 1;
    const UNKNOWN_DEVICE: u32 = 0;
    const fn fourcc(code: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*code)
    }
    const SCOPE_GLOBAL: u32 = fourcc(b"glob");
    const SCOPE_OUTPUT: u32 = fourcc(b"outp");
    const DEFAULT_OUTPUT_DEVICE: u32 = fourcc(b"dOut");
    const STREAM_FORMAT: u32 = fourcc(b"sfmt");
    const TRANSPORT_TYPE: u32 = fourcc(b"tran");

    #[repr(C)]
    struct PropertyAddress {
        selector: u32,
        scope: u32,
        element: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct StreamDescription {
        sample_rate: f64,
        format_id: u32,
        format_flags: u32,
        bytes_per_packet: u32,
        frames_per_packet: u32,
        bytes_per_frame: u32,
        channels_per_frame: u32,
        bits_per_channel: u32,
        reserved: u32,
    }

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectGetPropertyData(
            object: u32,
            address: *const PropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            data_size: *mut u32,
            data: *mut c_void,
        ) -> i32;
    }

    fn get<T: Default>(object: u32, selector: u32, scope: u32) -> Option<T> {
        let address = PropertyAddress {
            selector,
            scope,
            element: 0,
        };
        let mut value = T::default();
        let mut size = std::mem::size_of::<T>() as u32;
        // SAFETY: `value` is a live, writable `T` and `size` is its exact size;
        // CoreAudio writes at most `size` bytes and reports the count back.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                (&mut value as *mut T).cast(),
            )
        };
        (status == 0 && size as usize == std::mem::size_of::<T>()).then_some(value)
    }

    pub(super) fn read(adm_guid: &str) -> Option<OutputFormat> {
        let device = match adm_guid {
            "default" | "" => get::<u32>(SYSTEM_OBJECT, DEFAULT_OUTPUT_DEVICE, SCOPE_GLOBAL)?,
            id => id.parse::<u32>().ok()?,
        };
        if device == UNKNOWN_DEVICE {
            return None;
        }
        let format = get::<StreamDescription>(device, STREAM_FORMAT, SCOPE_OUTPUT)?;
        let transport = get::<u32>(device, TRANSPORT_TYPE, SCOPE_GLOBAL).unwrap_or(0);
        Some(OutputFormat {
            device,
            sample_rate_hz: format.sample_rate.round() as u32,
            channels: format.channels_per_frame,
            transport: DeviceClass::from_transport_fourcc(transport),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(device: u32, rate: u32, channels: u32) -> OutputFormat {
        OutputFormat {
            device,
            sample_rate_hz: rate,
            channels,
            transport: DeviceClass::Bluetooth,
        }
    }

    fn baselined(format: OutputFormat) -> PlayoutFormatTracker {
        let mut tracker = PlayoutFormatTracker::default();
        tracker.applied(Some(format));
        tracker
    }

    #[test]
    fn the_same_format_never_reapplies() {
        let mut tracker = baselined(fmt(87, 48_000, 2));
        for _ in 0..10 {
            assert_eq!(
                tracker.observe(Some(fmt(87, 48_000, 2))),
                FormatDecision::Unchanged
            );
        }
    }

    #[test]
    fn a_rate_change_reapplies_once_after_settling() {
        let a2dp = fmt(87, 48_000, 2);
        let hfp = fmt(87, 24_000, 1);
        let mut tracker = baselined(a2dp);
        assert_eq!(
            tracker.observe(Some(hfp)),
            FormatDecision::Settling {
                from: a2dp,
                to: hfp
            }
        );
        assert_eq!(
            tracker.observe(Some(hfp)),
            FormatDecision::Reapply {
                from: a2dp,
                to: hfp
            }
        );
        // The caller re-baselines after re-initializing playout.
        tracker.applied(Some(hfp));
        for _ in 0..5 {
            assert_eq!(tracker.observe(Some(hfp)), FormatDecision::Unchanged);
        }
    }

    #[test]
    fn a_stereo_rate_change_reapplies_the_measured_repro_shape() {
        // Mac Studio Speakers 48 -> 44.1 kHz, channels unchanged: the case the
        // ADM rejects as "Changing channels not supported".
        let before = fmt(87, 48_000, 2);
        let after = fmt(87, 44_100, 2);
        let mut tracker = baselined(before);
        tracker.observe(Some(after));
        assert_eq!(
            tracker.observe(Some(after)),
            FormatDecision::Reapply {
                from: before,
                to: after
            }
        );
    }

    #[test]
    fn a_change_that_reverts_before_confirming_does_nothing() {
        let mut tracker = baselined(fmt(87, 48_000, 2));
        assert!(matches!(
            tracker.observe(Some(fmt(87, 16_000, 1))),
            FormatDecision::Settling { .. }
        ));
        assert_eq!(
            tracker.observe(Some(fmt(87, 48_000, 2))),
            FormatDecision::Unchanged
        );
        assert!(matches!(
            tracker.observe(Some(fmt(87, 16_000, 1))),
            FormatDecision::Settling { .. }
        ));
    }

    #[test]
    fn a_burst_of_different_formats_waits_for_one_to_hold() {
        let mut tracker = baselined(fmt(87, 48_000, 2));
        assert!(matches!(
            tracker.observe(Some(fmt(87, 16_000, 1))),
            FormatDecision::Settling { .. }
        ));
        assert!(matches!(
            tracker.observe(Some(fmt(87, 24_000, 1))),
            FormatDecision::Settling { .. }
        ));
        assert!(matches!(
            tracker.observe(Some(fmt(87, 24_000, 1))),
            FormatDecision::Reapply { .. }
        ));
    }

    #[test]
    fn a_device_change_rebaselines_and_never_reapplies() {
        let speakers = fmt(87, 48_000, 2);
        let airpods = fmt(120, 24_000, 1);
        let mut tracker = baselined(speakers);
        assert_eq!(
            tracker.observe(Some(airpods)),
            FormatDecision::DeviceChanged {
                from: speakers,
                to: airpods
            }
        );
        assert_eq!(tracker.observe(Some(airpods)), FormatDecision::Unchanged);
    }

    #[test]
    fn an_unreadable_format_is_not_a_change_and_clears_a_pending_one() {
        let mut tracker = baselined(fmt(87, 48_000, 2));
        assert!(matches!(
            tracker.observe(Some(fmt(87, 24_000, 1))),
            FormatDecision::Settling { .. }
        ));
        assert_eq!(tracker.observe(None), FormatDecision::Unchanged);
        assert!(matches!(
            tracker.observe(Some(fmt(87, 24_000, 1))),
            FormatDecision::Settling { .. }
        ));
    }

    #[test]
    fn the_first_readable_format_only_baselines() {
        let mut tracker = PlayoutFormatTracker::default();
        assert_eq!(
            tracker.observe(Some(fmt(87, 24_000, 1))),
            FormatDecision::Unchanged
        );
        assert_eq!(
            tracker.observe(Some(fmt(87, 24_000, 1))),
            FormatDecision::Unchanged
        );
    }

    #[test]
    fn device_classes_bucket_coreaudio_transport_codes() {
        let code = |s: &[u8; 4]| u32::from_be_bytes(*s);
        assert_eq!(
            DeviceClass::from_transport_fourcc(code(b"blue")),
            DeviceClass::Bluetooth
        );
        assert_eq!(
            DeviceClass::from_transport_fourcc(code(b"bltn")),
            DeviceClass::BuiltIn
        );
        assert_eq!(
            DeviceClass::from_transport_fourcc(code(b"usb ")),
            DeviceClass::Usb
        );
        assert_eq!(DeviceClass::from_transport_fourcc(0), DeviceClass::Other);
        assert_eq!(
            fmt(87, 24_000, 1).to_string(),
            "class=bluetooth rate=24000Hz channels=1"
        );
    }

    #[test]
    fn a_guid_that_is_not_a_coreaudio_device_id_reads_nothing() {
        assert_eq!(read_output_format("not-a-device"), None);
    }
}
