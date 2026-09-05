//! Backend room-directory client for native-created room metadata.
//!
//! The native app owns local room persistence, but web participants learn the
//! room title from LiveKit room metadata. Native joins stamp the current
//! credential via `/api/rooms`; the backend preserves any existing server-side
//! knock-gate value and uses `open` only for first-create metadata.

use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum RoomDirectoryError {
    #[error("backend URL unavailable")]
    BackendUrlUnavailable,
    #[error("backend room metadata request timed out")]
    Timeout,
    #[error("backend room metadata connection failed")]
    Connect,
    #[error("backend room metadata transport failed")]
    Transport,
    #[error("backend room metadata endpoint returned HTTP {0}")]
    HttpStatus(reqwest::StatusCode),
}

fn directory_request_error(kind: super::backend_http::RequestErrorKind) -> RoomDirectoryError {
    match kind {
        super::backend_http::RequestErrorKind::Timeout => RoomDirectoryError::Timeout,
        super::backend_http::RequestErrorKind::Connect => RoomDirectoryError::Connect,
        super::backend_http::RequestErrorKind::Transport => RoomDirectoryError::Transport,
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackendRoomMetadataRequest<'a> {
    /// Human room display label. The backend stores it in LiveKit room metadata.
    name: &'a str,
    /// Existing native credential (`room-<32 hex>`) to stamp instead of
    /// generating a fresh backend credential.
    #[serde(skip_serializing_if = "Option::is_none")]
    room: Option<&'a str>,
    /// Initial knock-gate value for a newly created LiveKit room. The backend
    /// preserves server-side `open` when stamping an existing credential (#203).
    open: bool,
}

pub async fn ensure_room_metadata(
    room: &str,
    display_name: &str,
    initial_open: bool,
) -> Result<(), RoomDirectoryError> {
    let name = display_name.trim();
    if name.is_empty() {
        return Ok(());
    }

    let base = match crate::transport::token::backend_base_url() {
        Ok(base) => base,
        Err(err) => {
            #[cfg(any(test, debug_assertions))]
            if matches!(
                &err,
                crate::transport::token::TokenError::MissingEnv("PETAL_BACKEND_URL")
            ) {
                return Ok(());
            }
            let _ = err;
            return Err(RoomDirectoryError::BackendUrlUnavailable);
        }
    };
    let url = format!("{base}/api/rooms");
    let payload = BackendRoomMetadataRequest {
        name,
        room: Some(room),
        open: initial_open,
    };
    let response = super::backend_http::send_with_retry(
        super::backend_http::client().post(&url).json(&payload),
    )
    .await
    .map_err(|err| directory_request_error(super::backend_http::request_error_kind(&err)))?;

    if !response.status().is_success() {
        return Err(RoomDirectoryError::HttpStatus(response.status()));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ContractFixture {
        room_metadata_registration: RoomMetadataRegistration,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RoomMetadataRegistration {
        request: RoomMetadataRegistrationRequest,
        metadata: String,
    }

    #[derive(Deserialize)]
    struct RoomMetadataRegistrationRequest {
        name: String,
        room: String,
        open: bool,
    }

    fn contract_fixture() -> ContractFixture {
        serde_json::from_str(include_str!(
            "../../../../../contracts/petal-contracts.json"
        ))
        .unwrap()
    }

    #[test]
    fn backend_room_metadata_request_serializes_existing_credential_and_display_name() {
        let fixture = contract_fixture().room_metadata_registration;
        let request = BackendRoomMetadataRequest {
            name: &fixture.request.name,
            room: Some(&fixture.request.room),
            open: fixture.request.open,
        };

        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["name"], fixture.request.name);
        assert_eq!(value["room"], fixture.request.room);
        assert_eq!(value["open"], fixture.request.open);
        assert!(
            value.get("displayName").is_none(),
            "`name` is the existing /api/rooms display-label field"
        );
        assert_eq!(
            serde_json::json!({
                "displayName": fixture.request.name,
                "open": fixture.request.open,
            })
            .to_string(),
            fixture.metadata
        );
    }

    #[test]
    fn room_directory_errors_are_safe_when_join_logs_them() {
        // `session::join_room` logs this error verbatim before proceeding to
        // the token request, so every display form is an off-device boundary.
        assert_eq!(
            RoomDirectoryError::BackendUrlUnavailable.to_string(),
            "backend URL unavailable"
        );
        assert_eq!(
            directory_request_error(super::super::backend_http::RequestErrorKind::Timeout)
                .to_string(),
            "backend room metadata request timed out"
        );
        assert_eq!(
            directory_request_error(super::super::backend_http::RequestErrorKind::Connect)
                .to_string(),
            "backend room metadata connection failed"
        );
        assert_eq!(
            directory_request_error(super::super::backend_http::RequestErrorKind::Transport)
                .to_string(),
            "backend room metadata transport failed"
        );
        assert_eq!(
            RoomDirectoryError::HttpStatus(reqwest::StatusCode::BAD_GATEWAY).to_string(),
            "backend room metadata endpoint returned HTTP 502 Bad Gateway"
        );
    }
}
