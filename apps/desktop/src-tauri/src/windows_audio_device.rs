use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::Audio::{
    eCapture, eMultimedia, eRender, EDataFlow, IMMDeviceEnumerator, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

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
