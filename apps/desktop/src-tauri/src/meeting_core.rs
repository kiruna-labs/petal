//! Platform-independent room membership primitives.
//!
//! This module deliberately stops at a connected LiveKit room. Platform
//! services such as capture, compositor windows, audio devices, remote input,
//! and sleep assertions are attached by each platform's session module after
//! this shared connection step succeeds.

use std::sync::Arc;

use crate::rooms::{RoomRecord, RoomsState};
use crate::transport::publisher::RoomConnection;

/// Metadata registration is useful but non-fatal. Bound it independently so
/// an unavailable directory service cannot hide token/connect readiness.
const ROOM_METADATA_PREJOIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

/// The initial LiveKit connect has no client-side deadline of its own; on a
/// bad network a hung websocket dial would otherwise eat the whole join
/// budget with zero retries. Each attempt gets this bound.
const ROOM_CONNECT_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// One dropped dial must not fail the whole join (user-reported on a lossy
/// network, 2026-08-11: desktop "could not join room" while the web client,
/// which retries, eventually joined). Bounded: worst case adds ~23s, inside
/// the join terminal budget in `session/room.rs`.
const ROOM_CONNECT_RETRY_DELAYS: [std::time::Duration; 2] = [
    std::time::Duration::from_secs(1),
    std::time::Duration::from_secs(2),
];

#[derive(Debug, thiserror::Error, Clone, serde::Serialize)]
#[serde(tag = "kind", content = "message", rename_all = "camelCase")]
pub(crate) enum RoomJoinError {
    #[error("missing LiveKit configuration: {0}")]
    Config(String),
    #[error("failed to connect to LiveKit room: {0}")]
    RoomConnect(String),
}

pub(crate) struct ConnectedRoom {
    pub(crate) room_record: RoomRecord,
    pub(crate) room_connection: Arc<RoomConnection>,
    pub(crate) livekit_room_name: String,
    pub(crate) url: String,
}

/// Resolve or create the durable local room record before checking whether an
/// existing membership is already for this room. This preserves the existing
/// last-joined recency behavior even for an idempotent rejoin.
pub(crate) fn persist_joined_room_record(
    rooms: &RoomsState,
    room_name: &str,
) -> Result<RoomRecord, RoomJoinError> {
    rooms.create(room_name, true).map_err(|error| {
        log::warn!(
            "meeting_core: couldn't persist a joined record for '{}': {error}",
            crate::logging::log_safe_quoted(room_name)
        );
        RoomJoinError::Config(error.to_string())
    })
}

/// Mint credentials and establish the one transport connection shared by all
/// platform services. Capture/display permissions are intentionally absent:
/// joining a meeting must remain possible when those optional capabilities
/// are unavailable.
pub(crate) async fn connect_room(
    rooms: &RoomsState,
    mut room_record: RoomRecord,
    identity: &str,
    display_name: &str,
) -> Result<ConnectedRoom, RoomJoinError> {
    if let Some(room_display_name) = room_record
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.eq_ignore_ascii_case("room"))
    {
        match tokio::time::timeout(
            ROOM_METADATA_PREJOIN_BUDGET,
            crate::transport::room_directory::ensure_room_metadata(
                &room_record.name,
                room_display_name,
                room_record.open,
            ),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                log::warn!("meeting_core: room metadata registration failed before join: {error}");
            }
            Err(_) => {
                log::warn!(
                    "meeting_core: room metadata registration timed out after {}ms; continuing to token request",
                    ROOM_METADATA_PREJOIN_BUDGET.as_millis()
                );
            }
        }
    }

    let token_response =
        crate::transport::token::fetch_access_token(crate::transport::token::BackendTokenRequest {
            room: &room_record.name,
            identity,
            display_name: Some(display_name),
            access_code: room_record.access_code.as_deref(),
            can_publish: true,
            can_subscribe: true,
            can_publish_data: true,
            hidden: false,
        })
        .await
        .map_err(|error| {
            log::error!(
                "meeting_core: token request for '{}' failed: {error}",
                crate::logging::log_safe_quoted(&room_record.name)
            );
            crate::analytics::join_failed_from_token_error(&error);
            RoomJoinError::Config(error.to_string())
        })?;

    // AI chat presents this JWT to `/api/ai-token`, which verifies its
    // signature/room/identity before minting a Gemini token (#655). Held in
    // memory for the life of the join and cleared on leave. Cross-platform:
    // the Windows session (session_stub) joins through this same function.
    crate::ai_chat::room_auth::remember(token_response.token.clone());

    let room_connection = connect_with_bounded_retry(
        ROOM_CONNECT_ATTEMPT_TIMEOUT,
        &ROOM_CONNECT_RETRY_DELAYS,
        || RoomConnection::connect(&token_response.url, &token_response.token),
    )
    .await
    .map_err(|error| {
        let message = match error {
            BoundedConnectError::Attempt(error) => {
                crate::analytics::join_failed_from_connect_network();
                error.to_string()
            }
            BoundedConnectError::TimedOut => {
                crate::analytics::join_failed_from_connect_timeout();
                format!(
                    "connect attempt timed out after {}s",
                    ROOM_CONNECT_ATTEMPT_TIMEOUT.as_secs()
                )
            }
        };
        log::error!(
            "meeting_core: LiveKit connect for '{}' (transport room '{}') failed after retries: {message}",
            crate::logging::log_safe_quoted(&room_record.name),
            crate::logging::log_safe_quoted(&token_response.room)
        );
        RoomJoinError::RoomConnect(message)
    })?;

    room_record = room_record_with_learned_display_name(
        rooms,
        room_record,
        &room_connection.room().metadata(),
    );
    let room_connection = Arc::new(room_connection);

    Ok(ConnectedRoom {
        room_record,
        room_connection,
        livekit_room_name: token_response.room,
        url: token_response.url,
    })
}

#[derive(Debug)]
pub(crate) enum BoundedConnectError<E> {
    /// The attempt itself failed (and every retry after it).
    Attempt(E),
    /// The attempt hung past `ROOM_CONNECT_ATTEMPT_TIMEOUT` (and every retry
    /// after it).
    TimedOut,
}

/// Run a fallible connect attempt with a per-attempt timeout and bounded
/// retry delays between attempts. Total attempts = `retry_delays.len() + 1`.
/// Callers remain responsible for any overall deadline (the join terminal
/// budget in `session/room.rs` still caps the whole join).
pub(crate) async fn connect_with_bounded_retry<T, E, F, Fut>(
    attempt_timeout: std::time::Duration,
    retry_delays: &[std::time::Duration],
    mut attempt: F,
) -> Result<T, BoundedConnectError<E>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let attempts = retry_delays.len() + 1;
    for index in 0..attempts {
        let outcome = match tokio::time::timeout(attempt_timeout, attempt()).await {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(error)) => BoundedConnectError::Attempt(error),
            Err(_) => BoundedConnectError::TimedOut,
        };
        let Some(delay) = retry_delays.get(index) else {
            return Err(outcome);
        };
        match &outcome {
            BoundedConnectError::Attempt(error) => log::warn!(
                "meeting_core: room connect attempt {}/{attempts} failed ({error}); retrying in {:.1}s",
                index + 1,
                delay.as_secs_f32()
            ),
            BoundedConnectError::TimedOut => log::warn!(
                "meeting_core: room connect attempt {}/{attempts} timed out after {}s; retrying in {:.1}s",
                index + 1,
                attempt_timeout.as_secs(),
                delay.as_secs_f32()
            ),
        }
        tokio::time::sleep(*delay).await;
    }
    unreachable!("retry loop returns from the final attempt")
}

pub(crate) fn learned_display_name(
    metadata_json: &str,
    local_display: Option<&str>,
) -> Option<String> {
    if local_display
        .map(str::trim)
        .is_some_and(|name| !name.is_empty() && !is_generic_room_label(name))
    {
        return None;
    }

    let metadata: serde_json::Value = serde_json::from_str(metadata_json).ok()?;
    let display = metadata.get("displayName")?.as_str()?.trim();
    if display.is_empty() || is_generic_room_label(display) {
        return None;
    }

    Some(display.to_string())
}

pub(crate) fn room_record_with_learned_display_name(
    rooms: &RoomsState,
    room_record: RoomRecord,
    metadata_json: &str,
) -> RoomRecord {
    let Some(learned_display_name) =
        learned_display_name(metadata_json, room_record.display_name.as_deref())
    else {
        return room_record;
    };

    match rooms.rename_display(&room_record.name, Some(&learned_display_name)) {
        Ok(updated) => {
            log::info!(
                "meeting_core: learned display name '{}' for joined room '{}'",
                crate::logging::log_safe_quoted(&learned_display_name),
                crate::logging::log_safe_quoted(&room_record.name)
            );
            updated
        }
        Err(error) => {
            log::warn!(
                "meeting_core: couldn't persist learned display name for joined room '{}': {error}",
                crate::logging::log_safe_quoted(&room_record.name)
            );
            room_record
        }
    }
}

fn is_generic_room_label(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case("room")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time_util::now_ms;
    use std::path::PathBuf;
    use std::time::Duration;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "petal-meeting-core-test-{}-{}-{:?}",
            std::process::id(),
            now_ms(),
            std::thread::current().id()
        ))
    }

    fn wait_until_after_ms(timestamp_ms: u64) {
        while now_ms() <= timestamp_ms {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Paused-clock tests: sleeps/timeouts auto-advance instantly, so the
    /// retry ladder's real delays are exercised without real waiting.
    #[tokio::test(start_paused = true)]
    async fn connect_retry_succeeds_after_transient_failures() {
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempts.clone();
        let result = connect_with_bounded_retry(
            Duration::from_secs(10),
            &[Duration::from_secs(1), Duration::from_secs(2)],
            move || {
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if n < 2 {
                        Err("transient dial failure")
                    } else {
                        Ok("connected")
                    }
                }
            },
        )
        .await;

        assert_eq!(result.unwrap(), "connected");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn connect_retry_surfaces_the_final_attempts_error() {
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempts.clone();
        let result: Result<(), _> = connect_with_bounded_retry(
            Duration::from_secs(10),
            &[Duration::from_secs(1)],
            move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Err("still unreachable") }
            },
        )
        .await;

        match result {
            Err(BoundedConnectError::Attempt(message)) => {
                assert_eq!(message, "still unreachable");
            }
            other => panic!("expected the final attempt's error, got {other:?}"),
        }
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn connect_retry_bounds_a_hung_attempt_and_retries_it() {
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempts.clone();
        let result = connect_with_bounded_retry(
            Duration::from_secs(10),
            &[Duration::from_secs(1)],
            move || {
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if n == 0 {
                        // A dial that never completes: only the per-attempt
                        // timeout can end it.
                        std::future::pending::<()>().await;
                        unreachable!("pending future never resolves");
                    }
                    Ok::<_, &str>("connected on retry")
                }
            },
        )
        .await;

        assert_eq!(result.unwrap(), "connected on retry");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn connect_retry_reports_a_terminal_hang_as_timeout() {
        let result: Result<(), BoundedConnectError<&str>> =
            connect_with_bounded_retry(Duration::from_secs(10), &[], || async {
                std::future::pending::<Result<(), &str>>().await
            })
            .await;

        assert!(matches!(result, Err(BoundedConnectError::TimedOut)));
    }

    #[test]
    fn learned_name_fills_only_empty_or_generic_local_labels() {
        let metadata = r#"{"displayName":"Design sync","open":false}"#;
        assert_eq!(
            learned_display_name(metadata, Some("")).as_deref(),
            Some("Design sync")
        );
        assert_eq!(
            learned_display_name(metadata, Some(" room ")).as_deref(),
            Some("Design sync")
        );
        assert_eq!(learned_display_name(metadata, Some("Local name")), None);
    }

    #[test]
    fn joined_record_resolution_bumps_existing_room_recency() {
        let dir = temp_dir();
        let rooms = RoomsState::load(dir.clone());
        let first = rooms.create("abc-defg-hjk", true).unwrap();
        let first_joined_ms = first.last_joined_ms.expect("first join timestamp");
        wait_until_after_ms(first_joined_ms);

        let joined = persist_joined_room_record(&rooms, "abc-defg-hjk").unwrap();

        assert_eq!(first.id, joined.id);
        assert!(joined.last_joined_ms.unwrap() > first_joined_ms);
        assert_eq!(rooms.list().len(), 1);

        let _ = std::fs::remove_dir_all(dir);
    }
}
