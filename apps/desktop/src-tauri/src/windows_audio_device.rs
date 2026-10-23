use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eCapture, eMultimedia, eRender, EDataFlow, IMMDeviceEnumerator, MMDeviceEnumerator,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows_core::HSTRING;

struct ComApartment(bool);

impl ComApartment {
    fn enter() -> Result<Self, String> {
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized == RPC_E_CHANGED_MODE {
            return Ok(Self(false));
        }
        initialized
            .ok()
            .map_err(|error| format!("failed to initialize COM for audio devices: {error}"))?;
        Ok(Self(true))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

fn default_endpoint_id(flow: EDataFlow) -> Result<String, String> {
    let _apartment = ComApartment::enter()?;
    let enumerator: IMMDeviceEnumerator = unsafe {
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|error| format!("failed to create audio endpoint enumerator: {error}"))?
    };
    let endpoint = unsafe {
        enumerator
            .GetDefaultAudioEndpoint(flow, eMultimedia)
            .map_err(|error| format!("failed to read default audio endpoint: {error}"))?
    };
    let id = unsafe {
        endpoint
            .GetId()
            .map_err(|error| format!("failed to read default audio endpoint id: {error}"))?
    };
    let value = unsafe { id.to_string() }
        .map_err(|error| format!("default audio endpoint id is invalid UTF-16: {error}"));
    unsafe { CoTaskMemFree(Some(id.0.cast())) };
    value
}

pub(crate) fn default_recording_device_id() -> Result<String, String> {
    default_endpoint_id(eCapture)
}

pub(crate) fn default_playout_device_id() -> Result<String, String> {
    default_endpoint_id(eRender)
}

/// Ids of the endpoints Windows currently reports as `ACTIVE` for `flow`.
///
/// Used to keep devices that have genuinely gone away out of the lists we offer
/// the user: a Bluetooth headset that is switched off goes `NOTPRESENT` or
/// `UNPLUGGED`, an unplugged USB device goes `UNPLUGGED`, and one disabled in
/// Sound settings goes `DISABLED`. None of those should be selectable.
///
/// This filter does NOT catch a powered-off wireless headset: its USB dongle
/// stays `ACTIVE`, because Windows has no way to see that the headset on the
/// other end of the radio link is gone. During a call, `windows_mic_signal`
/// catches that case from the captured signal instead, and the user is asked
/// whether to move capture to another microphone.
fn active_endpoint_ids(flow: EDataFlow) -> Result<Vec<String>, String> {
    let _apartment = ComApartment::enter()?;
    let enumerator: IMMDeviceEnumerator = unsafe {
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|error| format!("failed to create audio endpoint enumerator: {error}"))?
    };
    let collection = unsafe {
        enumerator
            .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
            .map_err(|error| format!("failed to enumerate active audio endpoints: {error}"))?
    };
    let count = unsafe { collection.GetCount() }
        .map_err(|error| format!("failed to count active audio endpoints: {error}"))?;
    let mut ids = Vec::with_capacity(count as usize);
    for index in 0..count {
        let device = unsafe { collection.Item(index) }
            .map_err(|error| format!("failed to read active audio endpoint {index}: {error}"))?;
        let id = unsafe { device.GetId() }
            .map_err(|error| format!("failed to read active audio endpoint {index} id: {error}"))?;
        let value = unsafe { id.to_string() };
        unsafe { CoTaskMemFree(Some(id.0.cast())) };
        ids.push(value.map_err(|error| format!("audio endpoint id is invalid UTF-16: {error}"))?);
    }
    Ok(ids)
}

pub(crate) fn active_recording_endpoint_ids() -> Result<Vec<String>, String> {
    active_endpoint_ids(eCapture)
}

pub(crate) fn active_playout_endpoint_ids() -> Result<Vec<String>, String> {
    active_endpoint_ids(eRender)
}

/// Whether Windows itself has the recording endpoint `device_id` muted: a
/// laptop's mic-mute key, the mute toggle in Sound settings, or a headset's
/// own mute button. A muted endpoint captures exact digital silence, so
/// `windows_mic_signal` must not read it as a dead microphone.
pub(crate) fn recording_endpoint_muted(device_id: &str) -> Result<bool, String> {
    let _apartment = ComApartment::enter()?;
    let enumerator: IMMDeviceEnumerator = unsafe {
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|error| format!("failed to create audio endpoint enumerator: {error}"))?
    };
    let device = unsafe { enumerator.GetDevice(&HSTRING::from(device_id)) }
        .map_err(|error| format!("failed to open audio endpoint {device_id:?}: {error}"))?;
    let volume: IAudioEndpointVolume = unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|error| format!("failed to open the endpoint volume of {device_id:?}: {error}"))?;
    let muted = unsafe { volume.GetMute() }
        .map_err(|error| format!("failed to read the endpoint mute of {device_id:?}: {error}"))?;
    Ok(muted.as_bool())
}
