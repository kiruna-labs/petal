use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CaptureWindowPixelsResult {
    pub status: CaptureStatus,
    pub window_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rect: Option<PixelRect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CaptureStatus {
    Captured,
    Skipped,
}

pub(crate) fn capture_window_pixels(
    window_id: u32,
    rect: Option<PixelRect>,
    path: Option<String>,
) -> Result<CaptureWindowPixelsResult, String> {
    capture_window_pixels_with_preflight(
        window_id,
        rect,
        path,
        crate::platform::cg::frame_for_window_id(window_id).is_some(),
        crate::permissions::check_screen_recording(),
    )
}

fn capture_window_pixels_with_preflight(
    window_id: u32,
    rect: Option<PixelRect>,
    path: Option<String>,
    is_on_screen: bool,
    has_screen_recording: bool,
) -> Result<CaptureWindowPixelsResult, String> {
    if !is_on_screen {
        return Err(format!(
            "window {window_id} is not currently visible on screen"
        ));
    }
    if !has_screen_recording {
        return Ok(CaptureWindowPixelsResult {
            status: CaptureStatus::Skipped,
            window_id,
            path: None,
            rect,
            reason: Some("permission".to_string()),
        });
    }

    let output_path = resolve_output_path(window_id, path);
    capture_window_png(window_id, rect, &output_path)?;
    Ok(CaptureWindowPixelsResult {
        status: CaptureStatus::Captured,
        window_id,
        path: Some(output_path.display().to_string()),
        rect,
        reason: None,
    })
}

fn resolve_output_path(window_id: u32, path: Option<String>) -> PathBuf {
    match path.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "petal-window-pixels-{window_id}-{}.png",
                std::process::id()
            ));
            path
        }
    }
}

fn capture_window_png(
    window_id: u32,
    rect: Option<PixelRect>,
    output_path: &Path,
) -> Result<(), String> {
    if let Some(parent) = output_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create capture output directory: {e}"))?;
    }

    let Some(rect) = rect else {
        return run_screencapture(window_id, output_path);
    };

    validate_rect(rect)?;
    let mut tmp = std::env::temp_dir();
    tmp.push(format!(
        "petal-window-pixels-full-{window_id}-{}.png",
        std::process::id()
    ));
    run_screencapture(window_id, &tmp)?;
    let crop = Command::new("sips")
        .arg("-c")
        .arg(rect.height.to_string())
        .arg(rect.width.to_string())
        .arg("--cropOffset")
        .arg(rect.y.to_string())
        .arg(rect.x.to_string())
        .arg(&tmp)
        .arg("--out")
        .arg(output_path)
        .output()
        .map_err(|e| format!("failed to launch sips for crop: {e}"))?;
    let _ = std::fs::remove_file(&tmp);
    if !crop.status.success() {
        return Err(format!(
            "sips crop failed: {}",
            String::from_utf8_lossy(&crop.stderr)
        ));
    }
    Ok(())
}

fn validate_rect(rect: PixelRect) -> Result<(), String> {
    if rect.width == 0 || rect.height == 0 {
        return Err("capture rect width and height must be positive".to_string());
    }
    Ok(())
}

fn run_screencapture(window_id: u32, output_path: &Path) -> Result<(), String> {
    let output = Command::new("screencapture")
        .arg("-x")
        .arg("-o")
        .arg(format!("-l{window_id}"))
        .arg("-t")
        .arg("png")
        .arg(output_path)
        .output()
        .map_err(|e| format!("failed to launch screencapture: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "screencapture -l{window_id} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_preflight_refuses_hidden_window_before_permission_check() {
        let result = capture_window_pixels_with_preflight(42, None, None, false, false);
        assert_eq!(
            result,
            Err("window 42 is not currently visible on screen".to_string())
        );
    }

    #[test]
    fn capture_preflight_skips_when_screen_recording_missing() {
        let rect = PixelRect {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        };
        let result =
            capture_window_pixels_with_preflight(42, Some(rect), None, true, false).unwrap();

        assert_eq!(
            result,
            CaptureWindowPixelsResult {
                status: CaptureStatus::Skipped,
                window_id: 42,
                path: None,
                rect: Some(rect),
                reason: Some("permission".to_string()),
            }
        );
    }

    #[test]
    fn validates_non_empty_crop_rect() {
        assert_eq!(
            validate_rect(PixelRect {
                x: 0,
                y: 0,
                width: 0,
                height: 1,
            }),
            Err("capture rect width and height must be positive".to_string())
        );
    }
}
