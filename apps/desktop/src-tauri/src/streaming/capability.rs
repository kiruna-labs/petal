//! Which client-mode encode path this computer has, probed once per process.
//!
//! The go-live dialog shows it before the user tries (`StreamPreview`), and
//! go-live refuses a computer that can't encode at all. On Windows the probe
//! (`windows_encode::probe`) looks for a D3D11 video device and the Media
//! Foundation encoders; macOS always has VideoToolbox. The probe only informs:
//! each go-live still picks its own path when it opens the encoder.
//!
//! Also whether there is an audio output device: without one, Petal never
//! plays (or decodes) the meeting's audio, so other people aren't in the
//! stream. That one is checked live, since devices come and go.

use std::sync::OnceLock;

use serde::Serialize;

/// The encode path go-live will take here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ClientEncoder {
    /// A D3D11 video device (Windows) or VideoToolbox (macOS).
    Gpu,
    /// No D3D11 video device: convert on the CPU for the software H.264 MFT.
    Cpu,
    /// No usable H.264 or AAC encoder (a Windows N edition without the Media
    /// Feature Pack, for one).
    MissingEncoders,
    UnsupportedPlatform,
}

impl ClientEncoder {
    pub const fn wire(self) -> &'static str {
        match self {
            Self::Gpu => "gpu",
            Self::Cpu => "cpu",
            Self::MissingEncoders => "missingEncoders",
            Self::UnsupportedPlatform => "unsupportedPlatform",
        }
    }

    /// Whether going live can work here at all.
    pub const fn available(self) -> bool {
        matches!(self, Self::Gpu | Self::Cpu)
    }
}

/// What the Windows probe found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeFacts {
    /// `D3D11CreateDevice(HARDWARE, BGRA | VIDEO_SUPPORT)` succeeded and the
    /// device is an `ID3D11VideoDevice`.
    pub video_device: bool,
    /// Hardware H.264 encoder MFTs.
    pub hardware_h264: u32,
    /// A synchronous (software) H.264 encoder MFT.
    pub software_h264: bool,
    /// A synchronous AAC encoder MFT.
    pub aac: bool,
}

pub fn classify(facts: ProbeFacts) -> ClientEncoder {
    if !facts.aac {
        return ClientEncoder::MissingEncoders;
    }
    if facts.video_device && (facts.hardware_h264 > 0 || facts.software_h264) {
        return ClientEncoder::Gpu;
    }
    if !facts.video_device && facts.software_h264 {
        return ClientEncoder::Cpu;
    }
    ClientEncoder::MissingEncoders
}

/// Go-live's refusal for a computer that can't encode, if any.
pub fn refusal(encoder: ClientEncoder) -> Option<&'static str> {
    match encoder {
        ClientEncoder::Gpu | ClientEncoder::Cpu => None,
        ClientEncoder::MissingEncoders => Some(MISSING_ENCODERS_ERROR),
        ClientEncoder::UnsupportedPlatform => Some(UNSUPPORTED_PLATFORM_ERROR),
    }
}

pub const MISSING_ENCODERS_ERROR: &str = "Live streaming needs Windows' built-in video and audio encoders, which \
                                          this computer doesn't have. On Windows N editions, install the Media \
                                          Feature Pack.";
pub const UNSUPPORTED_PLATFORM_ERROR: &str = "Streaming from this computer isn't available on this platform yet";

#[cfg(target_os = "windows")]
fn probe() -> ClientEncoder {
    let facts = match std::thread::Builder::new()
        .name("petal-stream-probe".into())
        .spawn(super::windows_encode::probe)
        .map_err(|error| error.to_string())
        .and_then(|thread| thread.join().map_err(|_| "the probe panicked".to_string())?)
    {
        Ok(facts) => facts,
        Err(error) => {
            log::warn!("streaming: couldn't probe the encoders: {error}");
            ProbeFacts::default()
        }
    };
    let encoder = classify(facts);
    let yes_no = |value: bool| if value { "yes" } else { "no" };
    log::info!(
        "streaming: client encode support: video device {}, hardware H.264 {}, software H.264 {}, AAC {} -> {}",
        yes_no(facts.video_device),
        facts.hardware_h264,
        yes_no(facts.software_h264),
        yes_no(facts.aac),
        encoder.wire()
    );
    encoder
}

#[cfg(target_os = "macos")]
fn probe() -> ClientEncoder {
    ClientEncoder::Gpu
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn probe() -> ClientEncoder {
    ClientEncoder::UnsupportedPlatform
}

fn cache() -> &'static OnceLock<ClientEncoder> {
    static CACHE: OnceLock<ClientEncoder> = OnceLock::new();
    &CACHE
}

/// Probe in the background at startup, so the dialog never waits for it.
pub fn start_probe() {
    let _ = std::thread::Builder::new().name("petal-stream-capability".into()).spawn(client_encoder);
}

/// The cached result; blocks while the first probe runs.
pub fn client_encoder() -> ClientEncoder {
    *cache().get_or_init(probe)
}

/// [`client_encoder`] off the async runtime.
pub async fn client_encoder_async() -> ClientEncoder {
    tokio::task::spawn_blocking(client_encoder).await.unwrap_or(ClientEncoder::MissingEncoders)
}

/// Whether this computer has a default audio output device right now (a Mac
/// always does).
pub fn audio_output_available() -> bool {
    #[cfg(target_os = "windows")]
    {
        crate::windows_audio_device::default_playout_device_id().is_ok()
    }
    #[cfg(not(target_os = "windows"))]
    {
        true
    }
}

/// [`audio_output_available`] off the async runtime.
pub async fn audio_output_available_async() -> bool {
    tokio::task::spawn_blocking(audio_output_available).await.unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(video_device: bool, hardware_h264: u32, software_h264: bool, aac: bool) -> ProbeFacts {
        ProbeFacts { video_device, hardware_h264, software_h264, aac }
    }

    #[test]
    fn classifies_every_combination() {
        use ClientEncoder::*;
        for video_device in [false, true] {
            for hardware_h264 in [0, 2] {
                for software_h264 in [false, true] {
                    for aac in [false, true] {
                        let expected = match (video_device, hardware_h264 > 0, software_h264, aac) {
                            (_, _, _, false) => MissingEncoders,
                            (true, true, _, true) | (true, _, true, true) => Gpu,
                            (false, _, true, true) => Cpu,
                            _ => MissingEncoders,
                        };
                        let input = facts(video_device, hardware_h264, software_h264, aac);
                        assert_eq!(classify(input), expected, "{input:?}");
                    }
                }
            }
        }
        // The cases that matter, spelled out.
        assert_eq!(classify(facts(true, 1, true, true)), Gpu, "a GPU PC");
        assert_eq!(classify(facts(true, 0, true, true)), Gpu, "a video device, software H.264 only");
        assert_eq!(classify(facts(false, 0, true, true)), Cpu, "no video device (a VM, WARP)");
        assert_eq!(classify(facts(false, 1, false, true)), MissingEncoders, "a hardware MFT needs the device");
        assert_eq!(classify(facts(true, 1, true, false)), MissingEncoders, "no AAC (Windows N)");
        assert_eq!(classify(ProbeFacts::default()), MissingEncoders, "the probe failed");
    }

    #[test]
    fn go_live_refuses_only_where_nothing_can_encode() {
        assert_eq!(refusal(ClientEncoder::Gpu), None);
        assert_eq!(refusal(ClientEncoder::Cpu), None);
        assert_eq!(refusal(ClientEncoder::MissingEncoders), Some(MISSING_ENCODERS_ERROR));
        assert_eq!(refusal(ClientEncoder::UnsupportedPlatform), Some(UNSUPPORTED_PLATFORM_ERROR));
        assert!(ClientEncoder::Cpu.available() && !ClientEncoder::MissingEncoders.available());
        assert_eq!(serde_json::to_value(ClientEncoder::MissingEncoders).unwrap(), "missingEncoders");
        assert_eq!(serde_json::to_value(ClientEncoder::UnsupportedPlatform).unwrap(), "unsupportedPlatform");
    }
}
