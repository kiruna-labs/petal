//! Petal backend token client.
//!
//! Production app code must not read the LiveKit API secret. It asks the
//! backend for a scoped room token and signaling URL, keeping JWT signing on
//! the server side (issue #96/#97). The legacy local mint helper remains
//! available only for debug probes/tests.

#[cfg(any(test, debug_assertions))]
use livekit_api::access_token::{AccessToken, VideoGrants};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error(
        "no token backend is configured ({0} was not set at build time). This build cannot \
         mint tokens, so joining will always fail. Point it at your own backend and rebuild \
         -- see docs/SELF_HOSTING.md."
    )]
    MissingEnv(&'static str),
    #[error("invalid backend URL: {0}")]
    InvalidBackendUrl(String),
    #[error("backend token request failed: {0}")]
    Backend(String),
    #[error("backend token request timed out")]
    Timeout,
    #[error("backend token connection failed")]
    Connect,
    #[error("backend token transport failed")]
    Transport,
    #[error("backend token endpoint returned HTTP {0}")]
    HttpStatus(reqwest::StatusCode),
    #[error("backend token response was invalid")]
    Decode,
    #[error("token generation failed: {0}")]
    #[cfg(any(test, debug_assertions))]
    Jwt(#[from] livekit_api::access_token::AccessTokenError),
}

fn token_request_error(err: &reqwest::Error) -> TokenError {
    token_error_from_kind(super::backend_http::request_error_kind(err))
}

fn token_error_from_kind(kind: super::backend_http::RequestErrorKind) -> TokenError {
    match kind {
        super::backend_http::RequestErrorKind::Timeout => TokenError::Timeout,
        super::backend_http::RequestErrorKind::Connect => TokenError::Connect,
        super::backend_http::RequestErrorKind::Transport => TokenError::Transport,
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendTokenRequest<'a> {
    /// Human meeting code. The backend owns slug derivation so clients cannot
    /// accidentally double-prefix `petal-room-`.
    pub room: &'a str,
    pub identity: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<&'a str>,
    /// The invite access code this room's credential was derived from, when
    /// the local record still has it. The backend requires it for a room
    /// stamped `open: false` (knock-to-join) and ignores it otherwise -- see
    /// docs/CONTRACTS.md "Closed rooms and removed participants".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_code: Option<&'a str>,
    pub can_publish: bool,
    pub can_subscribe: bool,
    pub can_publish_data: bool,
    pub hidden: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendTokenResponse {
    pub url: String,
    pub token: String,
    pub room: String,
    pub display_name: Option<String>,
}

pub fn backend_base_url() -> Result<String, TokenError> {
    // Both the runtime env lookup AND the compile-time `option_env!` fallback
    // must filter out an empty-but-set value the same way. `option_env!`
    // bakes in whatever was present in the environment of whichever build
    // last recompiled this translation unit (cargo treats it as a tracked
    // compile input, so an env change alone can trigger a fresh bake) -- if
    // that build ran with `PETAL_BACKEND_URL=` (empty, e.g. a test harness
    // forcing the dev-mint fallback), the compiled binary bakes in `Some("")`
    // permanently, which used to skip the `MissingEnv` fallback entirely and
    // produce a schemeless `/api/token` URL (reqwest "builder error") instead
    // of correctly falling back to the local dev token mint.
    // An explicitly empty runtime value is a deliberate local-LiveKit opt-out
    // and must suppress the build-time default. An absent runtime value falls
    // back to the URL baked by build.rs.
    let raw = match std::env::var("PETAL_BACKEND_URL") {
        Ok(value) if value.trim().is_empty() => {
            return Err(TokenError::MissingEnv("PETAL_BACKEND_URL"));
        }
        Ok(value) => Some(value),
        Err(_) => option_env!("PETAL_BACKEND_URL")
            .map(str::to_string)
            .filter(|value| !value.trim().is_empty()),
    }
    .ok_or(TokenError::MissingEnv("PETAL_BACKEND_URL"))?;
    Ok(raw.trim().trim_end_matches('/').to_string())
}

pub async fn fetch_access_token(
    request: BackendTokenRequest<'_>,
) -> Result<BackendTokenResponse, TokenError> {
    let base = match backend_base_url() {
        Ok(base) => base,
        #[cfg(any(test, debug_assertions))]
        Err(TokenError::MissingEnv("PETAL_BACKEND_URL")) => {
            return mint_dev_access_token_response(request);
        }
        Err(e) => return Err(e),
    };
    let url = format!("{base}/api/token");
    let response = super::backend_http::send_with_retry(
        super::backend_http::client().post(&url).json(&request),
    )
    .await
    .map_err(|err| token_request_error(&err))?;

    if !response.status().is_success() {
        return Err(TokenError::HttpStatus(response.status()));
    }

    response
        .json::<BackendTokenResponse>()
        .await
        .map_err(|_| TokenError::Decode)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GalleryTokenRequest<'a> {
    room: &'a str,
    /// The caller's OWN visible-participant identity (no suffix). The
    /// backend derives and mints the `<base>-gallery` bridge identity itself
    /// after verifying this identity is a current participant in the room --
    /// see `backend/lib/handlers.ts::handleGalleryToken` (#109).
    base_identity: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<&'a str>,
}

/// Trusted, server-owned path for the hidden gallery-bridge participant
/// (#109/#26): unlike `fetch_access_token`, the caller does NOT get to pick
/// `hidden`/grant fields -- the backend hardcodes hidden/subscribe-only and
/// verifies `base_identity` is really in the room before minting anything.
/// See `gallery_bridge.rs`'s module doc for the full mechanism.
pub async fn fetch_gallery_access_token(
    room: &str,
    base_identity: &str,
    display_name: Option<&str>,
) -> Result<BackendTokenResponse, TokenError> {
    let base = match backend_base_url() {
        Ok(base) => base,
        #[cfg(any(test, debug_assertions))]
        Err(TokenError::MissingEnv("PETAL_BACKEND_URL")) => {
            // Dev fallback: mint the same hidden/subscribe-only grant locally
            // that the trusted backend endpoint would produce in production
            // (no participant-membership check available without the
            // backend, but this path never runs in a release build).
            let identity = format!("{base_identity}-gallery");
            return mint_dev_access_token_response(BackendTokenRequest {
                room,
                identity: &identity,
                display_name,
                access_code: None,
                can_publish: false,
                can_subscribe: true,
                can_publish_data: false,
                hidden: true,
            });
        }
        Err(e) => return Err(e),
    };
    let url = format!("{base}/api/gallery-token");
    let payload = GalleryTokenRequest {
        room,
        base_identity,
        display_name,
    };
    let response = super::backend_http::send_with_retry(
        super::backend_http::client().post(&url).json(&payload),
    )
    .await
    .map_err(|err| token_request_error(&err))?;

    if !response.status().is_success() {
        return Err(TokenError::HttpStatus(response.status()));
    }

    response
        .json::<BackendTokenResponse>()
        .await
        .map_err(|_| TokenError::Decode)
}

#[cfg(any(test, debug_assertions))]
fn mint_dev_access_token_response(
    request: BackendTokenRequest<'_>,
) -> Result<BackendTokenResponse, TokenError> {
    let url = livekit_url()?;
    let livekit_room = crate::rooms::livekit_room_name_for(request.room);
    log::warn!(
        "token: PETAL_BACKEND_URL missing; using DEV local token fallback for LiveKit room '{}' (release builds require the backend)",
        livekit_room
    );
    let token = mint_access_token_with_grants(
        request.identity,
        request.display_name.unwrap_or(request.identity),
        &livekit_room,
        request.can_publish,
        request.can_subscribe,
        request.can_publish_data,
        request.hidden,
    )?;

    Ok(BackendTokenResponse {
        url,
        token,
        room: livekit_room,
        display_name: None,
    })
}

/// Debug/test-only local token minting for probes and legacy transport tests.
#[cfg(any(test, debug_assertions))]
pub fn mint_access_token(
    identity: &str,
    room: &str,
    can_publish: bool,
    can_subscribe: bool,
) -> Result<String, TokenError> {
    mint_access_token_with_grants(
        identity,
        identity,
        room,
        can_publish,
        can_subscribe,
        true,
        false,
    )
}

#[cfg(any(test, debug_assertions))]
fn mint_access_token_with_grants(
    identity: &str,
    display_name: &str,
    room: &str,
    can_publish: bool,
    can_subscribe: bool,
    can_publish_data: bool,
    hidden: bool,
) -> Result<String, TokenError> {
    let api_key =
        std::env::var("LIVEKIT_API_KEY").map_err(|_| TokenError::MissingEnv("LIVEKIT_API_KEY"))?;
    let api_secret = std::env::var("LIVEKIT_API_SECRET")
        .map_err(|_| TokenError::MissingEnv("LIVEKIT_API_SECRET"))?;

    let token = AccessToken::with_api_key(&api_key, &api_secret)
        .with_identity(identity)
        .with_name(display_name)
        .with_grants(VideoGrants {
            room_join: true,
            room: room.to_string(),
            can_publish,
            can_subscribe,
            can_publish_data,
            can_update_own_metadata: true,
            hidden,
            ..Default::default()
        })
        .to_jwt()?;

    Ok(token)
}

/// Debug/test-only signaling URL helper for probes and legacy transport tests.
#[cfg(any(test, debug_assertions))]
pub fn livekit_url() -> Result<String, TokenError> {
    std::env::var("LIVEKIT_URL").map_err(|_| TokenError::MissingEnv("LIVEKIT_URL"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn env_lock() -> MutexGuard<'static, ()> {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn jwt_payload(token: &str) -> serde_json::Value {
        let payload = token
            .split('.')
            .nth(1)
            .expect("jwt should contain a payload segment");
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .expect("jwt payload should be base64url");
        serde_json::from_slice(&bytes).expect("jwt payload should be json")
    }

    #[test]
    fn backend_base_url_trims_space_and_trailing_slash() {
        let _guard = env_lock();
        std::env::set_var("PETAL_BACKEND_URL", "  https://petal.example.test/// ");

        assert_eq!(
            backend_base_url().unwrap(),
            "https://petal.example.test".to_string()
        );

        std::env::remove_var("PETAL_BACKEND_URL");
    }

    #[test]
    fn backend_base_url_uses_the_build_time_bake_when_runtime_env_is_absent() {
        let _guard = env_lock();
        std::env::remove_var("PETAL_BACKEND_URL");

        // There is no hosted fallback. An absent runtime value resolves to
        // whatever build.rs baked, and an unconfigured build baked nothing --
        // in which case this MUST report MissingEnv rather than quietly
        // pointing a third-party build at someone else's token service.
        match option_env!("PETAL_BACKEND_URL")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(baked) => assert_eq!(
                backend_base_url().unwrap(),
                baked.trim_end_matches('/').to_string()
            ),
            None => assert!(matches!(
                backend_base_url(),
                Err(TokenError::MissingEnv("PETAL_BACKEND_URL"))
            )),
        }
    }

    #[test]
    fn backend_token_request_serializes_least_privilege_grants() {
        let request = BackendTokenRequest {
            room: "eng-sync-0123456789abcdef0123456789abcdef",
            identity: "native-a",
            display_name: Some("Ada Lovelace"),
            access_code: None,
            can_publish: false,
            can_subscribe: true,
            can_publish_data: false,
            hidden: true,
        };

        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["room"], request.room);
        assert_eq!(value["identity"], request.identity);
        assert_eq!(value["displayName"], "Ada Lovelace");
        assert_eq!(value["canPublish"], false);
        assert_eq!(value["canSubscribe"], true);
        assert_eq!(value["canPublishData"], false);
        assert_eq!(value["hidden"], true);
        assert!(
            value.get("display_name").is_none(),
            "backend contract is camelCase"
        );
    }

    #[test]
    fn backend_token_request_carries_the_contract_access_code_for_closed_rooms() {
        let contracts: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../contracts/petal-contracts.json"
        )))
        .expect("contract fixtures should parse");
        let fixture = &contracts["closedRoomTokenRequest"]["request"];
        let request = BackendTokenRequest {
            room: fixture["room"].as_str().unwrap(),
            identity: fixture["identity"].as_str().unwrap(),
            display_name: fixture["displayName"].as_str(),
            access_code: fixture["accessCode"].as_str(),
            can_publish: true,
            can_subscribe: true,
            can_publish_data: true,
            hidden: false,
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["accessCode"], fixture["accessCode"]);
        assert_eq!(value["room"], fixture["room"]);
        assert!(value.get("access_code").is_none(), "backend contract is camelCase");

        let without = BackendTokenRequest { access_code: None, ..request };
        assert!(
            serde_json::to_value(&without).unwrap().get("accessCode").is_none(),
            "absent access code is omitted, not sent as null"
        );
    }

    #[test]
    fn backend_token_request_omits_absent_display_name() {
        let request = BackendTokenRequest {
            room: "eng-sync-0123456789abcdef0123456789abcdef",
            identity: "native-a",
            display_name: None,
            access_code: None,
            can_publish: true,
            can_subscribe: true,
            can_publish_data: true,
            hidden: false,
        };

        let value = serde_json::to_value(&request).unwrap();
        assert!(value.get("displayName").is_none());
    }

    #[test]
    fn token_request_failure_messages_are_structured_and_redacted() {
        assert_eq!(
            token_error_from_kind(super::super::backend_http::RequestErrorKind::Timeout)
                .to_string(),
            "backend token request timed out"
        );
        assert_eq!(
            token_error_from_kind(super::super::backend_http::RequestErrorKind::Connect)
                .to_string(),
            "backend token connection failed"
        );
        assert_eq!(
            token_error_from_kind(super::super::backend_http::RequestErrorKind::Transport)
                .to_string(),
            "backend token transport failed"
        );
        assert_eq!(
            TokenError::HttpStatus(reqwest::StatusCode::BAD_GATEWAY).to_string(),
            "backend token endpoint returned HTTP 502 Bad Gateway"
        );
        assert_eq!(
            TokenError::Decode.to_string(),
            "backend token response was invalid"
        );
    }

    #[test]
    fn mint_access_token_sets_identity_name_room_and_video_grants() {
        let _guard = env_lock();
        std::env::set_var("LIVEKIT_API_KEY", "devkey");
        std::env::set_var("LIVEKIT_API_SECRET", "devsecret");

        let token = mint_access_token("native-a", "petal-room-eng-sync", true, false).unwrap();
        let claims = jwt_payload(&token);

        assert_eq!(claims["iss"], "devkey");
        assert_eq!(claims["sub"], "native-a");
        assert_eq!(claims["name"], "native-a");
        assert_eq!(claims["video"]["roomJoin"], true);
        assert_eq!(claims["video"]["room"], "petal-room-eng-sync");
        assert_eq!(claims["video"]["canPublish"], true);
        assert_eq!(claims["video"]["canSubscribe"], false);
        assert_eq!(claims["video"]["canPublishData"], true);
        assert_eq!(claims["video"]["canUpdateOwnMetadata"], true);
        assert!(
            claims["exp"].as_i64().unwrap() > claims["nbf"].as_i64().unwrap(),
            "token should carry a positive validity window"
        );

        std::env::remove_var("LIVEKIT_API_KEY");
        std::env::remove_var("LIVEKIT_API_SECRET");
    }

    #[test]
    fn explicitly_empty_backend_url_uses_debug_local_token_fallback() {
        let _guard = env_lock();
        std::env::set_var("PETAL_BACKEND_URL", "");
        std::env::set_var("LIVEKIT_URL", "ws://localhost:7880");
        std::env::set_var("LIVEKIT_API_KEY", "devkey");
        std::env::set_var("LIVEKIT_API_SECRET", "devsecret");

        let request = BackendTokenRequest {
            room: "room-0123456789abcdef0123456789abcdef",
            identity: "native-a",
            display_name: Some("Ada Lovelace"),
            access_code: None,
            can_publish: false,
            can_subscribe: true,
            can_publish_data: false,
            hidden: true,
        };
        let response = futures::executor::block_on(fetch_access_token(request)).unwrap();
        let claims = jwt_payload(&response.token);

        assert_eq!(response.url, "ws://localhost:7880");
        assert_eq!(
            response.room,
            "petal-room-room-0123456789abcdef0123456789abcdef"
        );
        assert_eq!(claims["iss"], "devkey");
        assert_eq!(claims["sub"], "native-a");
        assert_eq!(claims["name"], "Ada Lovelace");
        assert_eq!(claims["video"]["room"], response.room);
        assert_eq!(claims["video"]["canPublish"], false);
        assert_eq!(claims["video"]["canSubscribe"], true);
        assert_eq!(claims["video"]["canPublishData"], false);
        assert_eq!(claims["video"]["hidden"], true);

        std::env::remove_var("LIVEKIT_URL");
        std::env::remove_var("LIVEKIT_API_KEY");
        std::env::remove_var("LIVEKIT_API_SECRET");
        std::env::remove_var("PETAL_BACKEND_URL");
    }

    #[test]
    fn gallery_token_request_serializes_base_identity_not_the_bridge_identity() {
        let request = GalleryTokenRequest {
            room: "eng-sync-0123456789abcdef0123456789abcdef",
            base_identity: "native-a",
            display_name: Some("native-a-gallery"),
        };

        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["room"], request.room);
        assert_eq!(value["baseIdentity"], "native-a");
        assert_eq!(value["displayName"], "native-a-gallery");
        assert!(
            value.get("identity").is_none(),
            "the bridge identity is derived server-side, never sent as `identity` (#109)"
        );
    }

    #[test]
    fn missing_backend_url_uses_debug_local_gallery_token_fallback() {
        let _guard = env_lock();
        std::env::set_var("PETAL_BACKEND_URL", "");
        std::env::set_var("LIVEKIT_URL", "ws://localhost:7880");
        std::env::set_var("LIVEKIT_API_KEY", "devkey");
        std::env::set_var("LIVEKIT_API_SECRET", "devsecret");

        let response = futures::executor::block_on(fetch_gallery_access_token(
            "room-0123456789abcdef0123456789abcdef",
            "native-a",
            Some("native-a-gallery"),
        ))
        .unwrap();
        let claims = jwt_payload(&response.token);

        assert_eq!(claims["sub"], "native-a-gallery");
        assert_eq!(claims["video"]["canPublish"], false);
        assert_eq!(claims["video"]["canSubscribe"], true);
        assert_eq!(claims["video"]["canPublishData"], false);
        assert_eq!(claims["video"]["hidden"], true);

        std::env::remove_var("LIVEKIT_URL");
        std::env::remove_var("LIVEKIT_API_KEY");
        std::env::remove_var("LIVEKIT_API_SECRET");
        std::env::remove_var("PETAL_BACKEND_URL");
    }
}
