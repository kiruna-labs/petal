// In apps/desktop/src-tauri/src/windows_screen_capture.rs around line 731:
if let Err(error) = session.SetIsBorderRequired(system_border_required) {
    // E_NOINTERFACE (0x80004002) means IGraphicsCaptureSession2 is not supported on this Windows build.
    // On these builds, the system capture border is already shown unconditionally by Windows.
    const E_NOINTERFACE: windows::core::HRESULT = windows::core::HRESULT(0x80004002_u32 as i32);
    
    if error.code() == E_NOINTERFACE {
        log::info!("SetIsBorderRequired not supported on this Windows build (E_NOINTERFACE); proceeding since system border is enabled by default.");
    } else if indicator_mode == CaptureIndicatorMode::Petal {
        log::warn!("Failed to configure WGC system indicator, falling back to Petal indicator: {error}");
        // ... fallback logic
    } else {
        let message = format!("required WGC system indicator could not be configured: {error}");
        set_terminal_error(state, &message);
        return Err(message.into());
    }
}