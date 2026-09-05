//! Shared Petal-backend HTTP plumbing (#143).
//!
//! Both the token client (`token.rs`) and the room-occupancy query (`rooms.rs`)
//! previously constructed a fresh `reqwest::Client` per request and open-coded
//! the "read a failed response body into a message" dance. This consolidates
//! the client (so the connection pool is reused) and the `{"error": "..."}`
//! envelope decoder in one place.

use std::sync::OnceLock;
use std::time::Duration;

static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// A single backend attempt must not leave an interactive join waiting forever.
/// Retries add their own bounded backoff below, so keep this deliberately short.
#[cfg(not(test))]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
// Test timeouts must sit WELL above local scheduler jitter: at 40ms/20ms a
// spuriously slow exchange counted as a timeout, retried against a test
// server whose thread had already exited, and failed the retry-policy tests
// with ConnectionRefused (~40% of `ci-local.sh` runs). 500ms keeps the
// stalled-server test bounded (4 attempts ~= 2.3s) without that race.
#[cfg(test)]
const REQUEST_TIMEOUT: Duration = Duration::from_millis(500);

#[cfg(not(test))]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(test)]
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// A process-wide reused `reqwest::Client`. Cloning a `reqwest::Client` is
/// cheap (it's an `Arc` internally) and shares the underlying connection pool,
/// unlike the previous `reqwest::Client::new()`-per-request pattern.
pub fn client() -> reqwest::Client {
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                .expect("backend HTTP client configuration is valid")
        })
        .clone()
}

#[cfg(not(test))]
fn retry_delays() -> [Duration; 3] {
    [
        Duration::from_millis(200),
        Duration::from_millis(600),
        Duration::from_millis(1500),
    ]
}

#[cfg(test)]
fn retry_delays() -> [Duration; 3] {
    [
        Duration::from_millis(1),
        Duration::from_millis(1),
        Duration::from_millis(1),
    ]
}

fn retry_jitter_ms(attempt: usize, url: &str) -> u64 {
    let hash = url
        .as_bytes()
        .iter()
        .fold(attempt as u64 * 17, |acc, byte| {
            acc.wrapping_mul(31).wrapping_add(*byte as u64)
        });
    hash % 75
}

fn retryable_reqwest_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect()
}

fn retry_http_status_log(
    status: reqwest::StatusCode,
    delay: Duration,
    attempt: usize,
    total: usize,
) {
    log::warn!(
        "backend request retry category=http-status status={} delay_ms={} attempt={}/{}",
        status.as_u16(),
        delay.as_millis(),
        attempt,
        total
    );
}

fn retry_transport_log(kind: RequestErrorKind, delay: Duration, attempt: usize, total: usize) {
    log::warn!(
        "backend request retry category={} delay_ms={} attempt={}/{}",
        kind.as_str(),
        delay.as_millis(),
        attempt,
        total
    );
}

/// A stable, privacy-safe classification for request failures. Callers must
/// not render the underlying `reqwest` error: it may include a configured URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestErrorKind {
    Timeout,
    Connect,
    Transport,
}

impl RequestErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Connect => "connect",
            Self::Transport => "transport",
        }
    }
}

pub fn request_error_kind(err: &reqwest::Error) -> RequestErrorKind {
    if err.is_timeout() {
        RequestErrorKind::Timeout
    } else if err.is_connect() {
        RequestErrorKind::Connect
    } else {
        RequestErrorKind::Transport
    }
}

/// Send a backend request with bounded retry/backoff for transient failures.
/// 4xx responses are never retried; retryable failures are connect/timeout
/// errors and 5xx responses (issue #217).
pub async fn send_with_retry(
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response, reqwest::Error> {
    // Set the total deadline on every clone rather than relying on a default
    // client deadline. This keeps all existing backend callers bounded too.
    let request = request.timeout(REQUEST_TIMEOUT);
    let template = request.try_clone();
    let url = request
        .try_clone()
        .and_then(|builder| builder.build().ok())
        .map(|built| built.url().to_string())
        .unwrap_or_else(|| "<unknown backend request>".to_string());
    let delays = retry_delays();
    let mut current = request;

    for attempt in 0..=delays.len() {
        match current.send().await {
            Ok(response) => {
                if response.status().is_server_error() && attempt < delays.len() {
                    let Some(next) = template.as_ref().and_then(|builder| builder.try_clone())
                    else {
                        return Ok(response);
                    };
                    let delay =
                        delays[attempt] + Duration::from_millis(retry_jitter_ms(attempt, &url));
                    retry_http_status_log(response.status(), delay, attempt + 1, delays.len());
                    tokio::time::sleep(delay).await;
                    current = next;
                    continue;
                }
                return Ok(response);
            }
            Err(err) => {
                if retryable_reqwest_error(&err) && attempt < delays.len() {
                    let Some(next) = template.as_ref().and_then(|builder| builder.try_clone())
                    else {
                        return Err(err);
                    };
                    let delay =
                        delays[attempt] + Duration::from_millis(retry_jitter_ms(attempt, &url));
                    retry_transport_log(request_error_kind(&err), delay, attempt + 1, delays.len());
                    tokio::time::sleep(delay).await;
                    current = next;
                    continue;
                }
                return Err(err);
            }
        }
    }

    unreachable!("retry loop always returns from the final attempt")
}

#[derive(serde::Deserialize)]
struct BackendErrorEnvelope {
    error: String,
}

/// Best-effort human message from a failed backend response: prefer the
/// `{"error": "..."}` envelope the backend sends, else the raw body, else just
/// the status line. Consumes the response (reads its body).
pub async fn error_message(response: reqwest::Response) -> String {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    serde_json::from_str::<BackendErrorEnvelope>(&body)
        .map(|parsed| parsed.error)
        .unwrap_or_else(|_| {
            if body.trim().is_empty() {
                status.to_string()
            } else {
                format!("{status}: {body}")
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    /// Bounded `TcpListener::accept()` (#724). A plain blocking `accept()`
    /// (or the `read()` on the stream it returns) can hang forever if the
    /// real-timer race between `CONNECT_TIMEOUT`/`REQUEST_TIMEOUT` and CI
    /// scheduler jitter ever desyncs the client's real connection count from
    /// what a test server thread expects -- that hung a CI job ~36 minutes
    /// instead of failing (issue #724). Every accepted stream also gets a
    /// read deadline so a connection that never sends bytes can't hang the
    /// server thread's `read()` either. Deadline errors just end the accept
    /// loop early rather than panicking: the test's own assertions (and, for
    /// `send_with_retry_bounds_each_stalled_attempt_and_classifies_timeout`,
    /// its outer `tokio::time::timeout`) are what actually verify behavior.
    fn accept_with_deadline(
        listener: &TcpListener,
        deadline: std::time::Instant,
    ) -> Option<std::net::TcpStream> {
        listener.set_nonblocking(true).ok()?;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    return Some(stream);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return None,
            }
        }
    }

    fn spawn_server(
        statuses: Vec<u16>,
    ) -> Option<(String, Arc<AtomicUsize>, thread::JoinHandle<()>)> {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping backend_http retry test: loopback listener denied by sandbox");
                return None;
            }
            Err(err) => panic!("failed to bind loopback test server: {err}"),
        };
        let url = format!("http://{}", listener.local_addr().unwrap());
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_thread = attempts.clone();
        let handle = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            for status in statuses {
                let Some(mut stream) = accept_with_deadline(&listener, deadline) else {
                    return;
                };
                attempts_for_thread.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0_u8; 1024];
                let _ = stream.read(&mut buf);
                let reason = if status == 200 { "OK" } else { "ERROR" };
                let body = if status == 200 {
                    r#"{"ok":true}"#
                } else {
                    r#"{"error":"temporary backend failure"}"#
                };
                write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        Some((url, attempts, handle))
    }

    fn spawn_stalled_server() -> Option<(String, thread::JoinHandle<()>)> {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping backend_http timeout test: loopback listener denied by sandbox"
                );
                return None;
            }
            Err(err) => panic!("failed to bind loopback test server: {err}"),
        };
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            // Accept each retry but intentionally never send HTTP headers.
            // This is the exact class of stalled backend request #555 guards.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut streams = Vec::new();
            for _ in 0..4 {
                let Some(mut stream) = accept_with_deadline(&listener, deadline) else {
                    break;
                };
                let mut buf = [0_u8; 1024];
                let _ = stream.read(&mut buf);
                streams.push(stream);
            }
            // Outlive the client's final attempt: dropping these streams
            // before its REQUEST_TIMEOUT expires turns the stall into a
            // connection-reset transport error instead of a timeout.
            thread::sleep(REQUEST_TIMEOUT * 2);
        });
        Some((url, handle))
    }

    /// #724: bound every `send_with_retry` call in this test module so a
    /// stuck retry/connect race fails the test in seconds instead of hanging
    /// the whole CI job (a real stalled-server race once hung a macOS runner
    /// for ~36 minutes with no other signal). This is on top of, not instead
    /// of, `accept_with_deadline` -- two independent backstops on the same
    /// failure class.
    async fn bounded_send_with_retry(
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, reqwest::Error> {
        tokio::time::timeout(Duration::from_secs(5), send_with_retry(request))
            .await
            .expect("send_with_retry must not exceed the test's bounded deadline (#724)")
    }

    #[tokio::test]
    async fn send_with_retry_retries_5xx_then_succeeds() {
        let Some((url, attempts, handle)) = spawn_server(vec![500, 500, 200]) else {
            return;
        };

        let response = bounded_send_with_retry(client().post(&url).json(&serde_json::json!({
            "room": "eng-sync-0123456789abcdef0123456789abcdef"
        })))
        .await
        .unwrap();

        assert!(response.status().is_success());
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn send_with_retry_does_not_retry_4xx() {
        let Some((url, attempts, handle)) = spawn_server(vec![400]) else {
            return;
        };

        let response = bounded_send_with_retry(client().post(&url).json(&serde_json::json!({
            "room": "eng-sync-0123456789abcdef0123456789abcdef"
        })))
        .await
        .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn send_with_retry_bounds_each_stalled_attempt_and_classifies_timeout() {
        let Some((url, handle)) = spawn_stalled_server() else {
            return;
        };
        let started = std::time::Instant::now();
        let error = bounded_send_with_retry(client().post(&url)).await.unwrap_err();

        assert!(
            error.is_timeout(),
            "stalled response must be a timeout: {error}"
        );
        assert_eq!(request_error_kind(&error), RequestErrorKind::Timeout);
        // 4 attempts x REQUEST_TIMEOUT(500ms) + retry delays/jitter ~= 2.3s.
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "test timeout budget must stay bounded, elapsed={:?}",
            started.elapsed()
        );
        handle.join().unwrap();
    }

    #[test]
    fn request_error_kind_labels_are_stable_and_safe() {
        assert_eq!(RequestErrorKind::Timeout.as_str(), "timeout");
        assert_eq!(RequestErrorKind::Connect.as_str(), "connect");
        assert_eq!(RequestErrorKind::Transport.as_str(), "transport");
    }

    #[test]
    fn retry_diagnostics_never_format_raw_backend_url_or_reqwest_error() {
        // Keep jitter's private URL fingerprint separate from diagnostics: a
        // configured endpoint can contain sensitive path/query material.
        let source = include_str!("backend_http.rs");
        let url_placeholder = ["{", "url", "}"].concat();
        let error_placeholder = ["{", "err", "}"].concat();
        assert!(!source.contains(&format!("backend request to {url_placeholder}")));
        assert!(!source.contains(&format!("failed transiently: {error_placeholder}")));
        assert!(source.contains("category=http-status"));
        assert!(source.contains("category={}"));
    }
}
