//! Tauri command surface for the UserDispatch feedback modal's optional,
//! redacted diagnostic-log attachment (#292).
//!
//! **No hosted widget/script is ever loaded by this crate.** The frontend
//! bundles `@userdispatch/sdk` directly and only imports/calls it when a
//! public `VITE_USERDISPATCH_PUBLIC_KEY` was baked into the build (see
//! `apps/desktop/src/lib/feedback/config.ts` and `userDispatch.ts`) -- this
//! module never talks to UserDispatch itself, it only produces the OPT-IN
//! attachment bytes the frontend may choose to send.
//!
//! **Screenshare-recursion (#292 point 6):** the main Petal window this
//! modal lives in can never itself be a shareable capture source --
//! `window_source.rs` excludes every window owned by this process from
//! share-source enumeration (see
//! `own_process_windows_are_excluded_from_share_source_enumeration`), so
//! there is no capture-recursion risk to guard against here. The guard
//! below (rejecting `prepare_feedback_diagnostics` while ANY window IS being
//! shared, and the frontend closing/disabling the modal in the same
//! situation) is a separate, narrower privacy courtesy: don't invite
//! attaching fresh diagnostics while the user is mid-share.

use base64::Engine;
use serde::Serialize;

use crate::logging;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackDiagnostics {
    pub filename: String,
    pub mime_type: String,
    pub bytes_base64: String,
    pub byte_count: usize,
}

/// Builds the bounded, redacted diagnostic attachment (#292 points 3/4) and
/// base64-encodes it for IPC transfer to the webview. No path or temp-file
/// handle is ever returned -- only opaque bytes, or a typed error.
///
/// Checked against an active share TWICE in the overall flow: once here
/// (before any file I/O, so a share in progress skips building the archive
/// entirely) and once more by the caller immediately before it hands the
/// bytes to the UserDispatch SDK (`FeedbackModal.svelte` rechecks
/// `sharedWindowIds` right before submit). Neither check alone is airtight
/// against a share starting mid-flight -- a genuinely atomic guard would
/// need a session-lifecycle event owned by #298's exclusive
/// `session/share.rs`/`resilience.rs` lock, which is out of scope for this
/// change -- but together they close the practical window without touching
/// those files.
#[tauri::command]
pub async fn prepare_feedback_diagnostics(
    state: tauri::State<'_, crate::session::SessionState>,
) -> Result<FeedbackDiagnostics, String> {
    if !state.active_share_ids().is_empty() {
        return Err("sharing_active".to_string());
    }
    let bytes = tokio::task::spawn_blocking(logging::build_feedback_attachment_zip)
        .await
        .map_err(|e| format!("feedback diagnostics task failed: {e}"))??;
    Ok(FeedbackDiagnostics {
        filename: "petal-feedback-diagnostics.zip".to_string(),
        mime_type: "application/zip".to_string(),
        byte_count: bytes.len(),
        bytes_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_diagnostics_serializes_camel_case_for_the_frontend() {
        let diagnostics = FeedbackDiagnostics {
            filename: "petal-feedback-diagnostics.zip".into(),
            mime_type: "application/zip".into(),
            bytes_base64: "AA==".into(),
            byte_count: 1,
        };
        let json = serde_json::to_string(&diagnostics).unwrap();
        assert!(json.contains("\"filename\""));
        assert!(json.contains("\"mimeType\""));
        assert!(json.contains("\"bytesBase64\""));
        assert!(json.contains("\"byteCount\""));
    }
}
