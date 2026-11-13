//! RTMP(S) publishing for client mode. `shiguredo_rtmp` is sans-I/O; this
//! module owns the socket: TCP via tokio, RTMPS via tokio-native-tls, and the
//! reconnect schedule.
//!
//! The stream key is a secret. It lives only in [`RtmpDestination`] (whose
//! `Debug` redacts it) and the library's connection object, which is never
//! formatted. Errors that might echo it back from a server are scrubbed by
//! [`RtmpDestination::redact`] before they leave this module.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use shiguredo_rtmp::{
    RtmpConnectionEvent, RtmpConnectionState, RtmpPublishClientConnection, RtmpUrl,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use super::flv::MediaPacket;
use super::queue::{MediaQueue, ReconnectPolicy, RECONNECT_DELAYS};

pub const RTMP_URL_MAX_LEN: usize = 512;
pub const RTMP_KEY_MAX_LEN: usize = 256;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Where to publish. The server URL is `rtmp[s]://host[:port]/app`; the key
/// is the stream name.
#[derive(Clone, PartialEq, Eq)]
pub struct RtmpDestination {
    server_url: String,
    stream_key: String,
}

impl fmt::Debug for RtmpDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RtmpDestination")
            .field("server_url", &self.server_url)
            .field("stream_key", &"[redacted]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DestinationError {
    #[error("The server URL must start with rtmp:// or rtmps://")]
    Scheme,
    #[error("The server URL is too long")]
    UrlTooLong,
    #[error("The stream key is too long")]
    KeyTooLong,
    #[error("Enter a stream key")]
    MissingKey,
    #[error("The server URL must not contain a user name or password")]
    Credentials,
    #[error("The server URL contains your stream key. Put the key only in the Stream key field.")]
    KeyInUrl,
    #[error("The server URL is not valid")]
    Invalid,
}

impl RtmpDestination {
    pub fn new(server_url: &str, stream_key: &str) -> Result<Self, DestinationError> {
        let server_url = server_url.trim().trim_end_matches('/').to_string();
        let stream_key = stream_key.trim().to_string();
        if server_url.len() > RTMP_URL_MAX_LEN {
            return Err(DestinationError::UrlTooLong);
        }
        if stream_key.len() > RTMP_KEY_MAX_LEN {
            return Err(DestinationError::KeyTooLong);
        }
        if stream_key.is_empty() {
            return Err(DestinationError::MissingKey);
        }
        let lower = server_url.to_ascii_lowercase();
        if !(lower.starts_with("rtmp://") || lower.starts_with("rtmps://")) {
            return Err(DestinationError::Scheme);
        }
        let authority = server_url
            .split_once("://")
            .map(|(_, rest)| rest.split('/').next().unwrap_or(""))
            .unwrap_or("");
        if authority.contains('@') {
            return Err(DestinationError::Credentials);
        }
        // The URL is saved in plain text and shown on screen; the key is a
        // secret kept in the keychain. Refuse a URL that carries it.
        if url_contains_key(&server_url, &stream_key) {
            return Err(DestinationError::KeyInUrl);
        }
        // Normalise the scheme's case for the parser.
        let (scheme, rest) = server_url.split_once("://").unwrap_or(("", ""));
        let server_url = format!("{}://{rest}", scheme.to_ascii_lowercase());
        let destination = Self { server_url, stream_key };
        destination.rtmp_url().map_err(|_| DestinationError::Invalid)?;
        Ok(destination)
    }

    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    pub fn stream_key(&self) -> &str {
        &self.stream_key
    }

    fn rtmp_url(&self) -> Result<RtmpUrl, shiguredo_rtmp::Error> {
        RtmpUrl::parse_with_stream_name(&self.server_url, &self.stream_key)
    }

    /// `text` with every occurrence of the stream key removed.
    pub fn redact(&self, text: &str) -> String {
        if self.stream_key.is_empty() {
            text.to_string()
        } else {
            text.replace(&self.stream_key, "[redacted]")
        }
    }
}

/// Keys shorter than this only match a whole path segment or query value,
/// so a short key never collides with an ordinary word in the URL.
const KEY_SUBSTRING_MIN_LEN: usize = 8;

/// Whether `server_url` carries `stream_key`: as a whole path segment or
/// query value, or anywhere once the key is long enough to be unmistakable.
fn url_contains_key(server_url: &str, stream_key: &str) -> bool {
    let rest = server_url.split_once("://").map_or(server_url, |(_, rest)| rest);
    let (_, after_authority) = rest.split_once('/').unwrap_or((rest, ""));
    after_authority.split(['/', '?', '&', '=']).any(|part| part == stream_key)
        || (stream_key.len() >= KEY_SUBSTRING_MIN_LEN && rest.contains(stream_key))
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RtmpError {
    #[error("Couldn't reach the streaming server: {0}")]
    Connect(String),
    #[error("Secure connection to the streaming server failed: {0}")]
    Tls(String),
    #[error("The streaming server didn't accept the stream: {0}")]
    Rejected(String),
    #[error("The streaming server closed the connection: {0}")]
    Dropped(String),
    #[error("The streaming server took too long to respond")]
    Timeout,
    #[error("The connection to the streaming server dropped")]
    Closed,
    #[error("The network can't keep up with the stream")]
    Stalled,
    #[error("RTMP protocol error: {0}")]
    Protocol(String),
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

async fn open_socket(destination: &RtmpDestination) -> Result<Box<dyn Io>, RtmpError> {
    let url = destination.rtmp_url().map_err(|_| RtmpError::Connect("invalid URL".into()))?;
    let tcp = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect((url.host.as_str(), url.port)),
    )
    .await
    .map_err(|_| RtmpError::Timeout)?
    .map_err(|error| RtmpError::Connect(error.kind().to_string()))?;
    let _ = tcp.set_nodelay(true);
    if !url.tls {
        return Ok(Box::new(tcp));
    }
    let connector = tokio_native_tls::native_tls::TlsConnector::new()
        .map_err(|error| RtmpError::Tls(error.to_string()))?;
    let tls = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio_native_tls::TlsConnector::from(connector).connect(&url.host, tcp),
    )
    .await
    .map_err(|_| RtmpError::Timeout)?
    .map_err(|error| RtmpError::Tls(destination.redact(&error.to_string())))?;
    Ok(Box::new(tls))
}

fn peer_disconnect_reason(
    connection: &mut RtmpPublishClientConnection,
    destination: &RtmpDestination,
) -> Option<String> {
    let mut reason = None;
    while let Some(event) = connection.next_event() {
        if let RtmpConnectionEvent::DisconnectedByPeer { reason: why } = event {
            reason = Some(destination.redact(&why));
        }
    }
    reason
}

async fn flush(
    connection: &mut RtmpPublishClientConnection,
    writer: &mut (impl AsyncWrite + Unpin),
    bytes_sent: &AtomicU64,
) -> Result<(), RtmpError> {
    while !connection.send_buf().is_empty() {
        let written = tokio::time::timeout(WRITE_TIMEOUT, writer.write(connection.send_buf()))
            .await
            .map_err(|_| RtmpError::Stalled)?
            .map_err(|_| RtmpError::Closed)?;
        if written == 0 {
            return Err(RtmpError::Closed);
        }
        connection.advance_send_buf(written);
        bytes_sent.fetch_add(written as u64, Ordering::Relaxed);
    }
    tokio::time::timeout(WRITE_TIMEOUT, writer.flush())
        .await
        .map_err(|_| RtmpError::Stalled)?
        .map_err(|_| RtmpError::Closed)
}

/// One connected, publishing session.
struct Session {
    reader: tokio::io::ReadHalf<Box<dyn Io>>,
    writer: tokio::io::WriteHalf<Box<dyn Io>>,
    connection: RtmpPublishClientConnection,
}

async fn connect_and_publish(
    destination: &RtmpDestination,
    bytes_sent: &AtomicU64,
) -> Result<Session, RtmpError> {
    let io = open_socket(destination).await?;
    let (mut reader, mut writer) = tokio::io::split(io);
    let url = destination.rtmp_url().map_err(|_| RtmpError::Connect("invalid URL".into()))?;
    let mut connection = RtmpPublishClientConnection::new(url);
    let handshake = async {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            flush(&mut connection, &mut writer, bytes_sent).await?;
            match connection.state() {
                RtmpConnectionState::Publishing => return Ok(()),
                RtmpConnectionState::Disconnecting => {
                    let reason = peer_disconnect_reason(&mut connection, destination)
                        .unwrap_or_else(|| "disconnected".into());
                    return Err(RtmpError::Rejected(reason));
                }
                _ => {}
            }
            let read = reader.read(&mut buf).await.map_err(|_| RtmpError::Closed)?;
            if read == 0 {
                return Err(RtmpError::Closed);
            }
            connection
                .feed_recv_buf(&buf[..read])
                .map_err(|error| RtmpError::Protocol(destination.redact(&error.reason)))?;
        }
    };
    tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
        .await
        .map_err(|_| RtmpError::Timeout)??;
    // Drain handshake chatter so later reads only report fresh events.
    let _ = peer_disconnect_reason(&mut connection, destination);
    Ok(Session { reader, writer, connection })
}

/// The latest sequence headers seen, resent first on every new connection.
#[derive(Debug, Default)]
struct HeaderCache {
    video: Option<MediaPacket>,
    audio: Option<MediaPacket>,
}

impl HeaderCache {
    fn observe(&mut self, packet: &MediaPacket) {
        if packet.is_sequence_header() {
            if packet.is_video() {
                self.video = Some(packet.clone());
            } else {
                self.audio = Some(packet.clone());
            }
        }
    }
}

fn send_packet(
    connection: &mut RtmpPublishClientConnection,
    packet: MediaPacket,
    destination: &RtmpDestination,
) -> Result<(), RtmpError> {
    let result = if packet.is_video() {
        packet.into_video_frame().map(|frame| connection.send_video(frame))
    } else {
        packet.into_audio_frame().map(|frame| connection.send_audio(frame))
    };
    match result {
        Some(Ok(())) | None => Ok(()),
        Some(Err(error)) => Err(RtmpError::Protocol(destination.redact(&error.reason))),
    }
}

/// Pump the queue into one session until it fails or is cancelled.
async fn pump(
    session: &mut Session,
    queue: &MediaQueue,
    headers: &mut HeaderCache,
    destination: &RtmpDestination,
    bytes_sent: &AtomicU64,
    cancel: &CancellationToken,
) -> Result<(), RtmpError> {
    // Each connection is a fresh RTMP stream: rebase timestamps to zero and
    // lead with the sequence headers so the server can decode what follows.
    let mut base_ts: Option<u32> = None;
    for header in [headers.video.clone(), headers.audio.clone()].into_iter().flatten() {
        send_packet(&mut session.connection, header.with_ts(0), destination)?;
    }
    let mut buf = vec![0u8; 16 * 1024];
    let mut stall_check = tokio::time::interval(Duration::from_millis(500));
    loop {
        flush(&mut session.connection, &mut session.writer, bytes_sent).await?;
        if session.connection.state() != RtmpConnectionState::Publishing {
            let reason = peer_disconnect_reason(&mut session.connection, destination);
            return Err(reason.map_or(RtmpError::Closed, RtmpError::Dropped));
        }
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(()),
            read = session.reader.read(&mut buf) => {
                let read = read.map_err(|_| RtmpError::Closed)?;
                if read == 0 {
                    return Err(RtmpError::Closed);
                }
                session
                    .connection
                    .feed_recv_buf(&buf[..read])
                    .map_err(|error| RtmpError::Protocol(destination.redact(&error.reason)))?;
            }
            packet = queue.pop() => {
                let Some(packet) = packet else { return Ok(()) };
                headers.observe(&packet);
                let base = *base_ts.get_or_insert(packet.ts_ms());
                let ts = packet.ts_ms().saturating_sub(base);
                send_packet(&mut session.connection, packet.with_ts(ts), destination)?;
            }
            _ = stall_check.tick() => {
                if queue.is_stalled() {
                    return Err(RtmpError::Stalled);
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublisherState {
    Connecting,
    Live,
    Reconnecting { attempt: u32 },
    Stopped,
    Failed(String),
}

#[derive(Debug, Default)]
pub struct PublisherStats {
    pub bytes_sent: AtomicU64,
    pub connected: AtomicBool,
}

/// Connect, publish `queue` until cancelled, and reconnect on failure with
/// the 1/3/9 s schedule. A first connection that fails is final (a wrong
/// URL or key should surface at once, not after 13 s of retries).
pub async fn run_publisher(
    destination: RtmpDestination,
    queue: Arc<MediaQueue>,
    stats: Arc<PublisherStats>,
    state_tx: tokio::sync::watch::Sender<PublisherState>,
    cancel: CancellationToken,
    reconnect_delays: Option<&'static [Duration]>,
) -> PublisherState {
    let delays = reconnect_delays.unwrap_or(&RECONNECT_DELAYS);
    let mut policy = ReconnectPolicy::with_delays(delays);
    let mut headers = HeaderCache::default();
    let mut ever_connected = false;
    let finish = |state: PublisherState| {
        stats.connected.store(false, Ordering::Relaxed);
        let _ = state_tx.send(state.clone());
        state
    };
    let _ = state_tx.send(PublisherState::Connecting);
    loop {
        let connected = tokio::select! {
            () = cancel.cancelled() => return finish(PublisherState::Stopped),
            result = connect_and_publish(&destination, &stats.bytes_sent) => result,
        };
        let (error, connected_for) = match connected {
            Ok(mut session) => {
                ever_connected = true;
                stats.connected.store(true, Ordering::Relaxed);
                let _ = state_tx.send(PublisherState::Live);
                let up_since = Instant::now();
                let result =
                    pump(&mut session, &queue, &mut headers, &destination, &stats.bytes_sent, &cancel)
                        .await;
                stats.connected.store(false, Ordering::Relaxed);
                let _ = session.writer.shutdown().await;
                match result {
                    Ok(()) => return finish(PublisherState::Stopped),
                    Err(error) => (error, Some(up_since.elapsed())),
                }
            }
            Err(error) => (error, None),
        };
        if cancel.is_cancelled() {
            return finish(PublisherState::Stopped);
        }
        if !ever_connected {
            return finish(PublisherState::Failed(error.to_string()));
        }
        let Some(delay) = policy.next_delay(connected_for) else {
            return finish(PublisherState::Failed(error.to_string()));
        };
        let attempt = policy.attempts_used();
        log::warn!("streaming: RTMP connection lost ({error}); reconnect attempt {attempt} in {delay:?}");
        let _ = state_tx.send(PublisherState::Reconnecting { attempt });
        tokio::select! {
            () = cancel.cancelled() => return finish(PublisherState::Stopped),
            () = tokio::time::sleep(delay) => {}
        }
        queue.reset_for_reconnect();
    }
}

/// An in-process RTMP server for tests: accepts publishes, records media.
#[cfg(test)]
pub(crate) mod test_server {
    use std::sync::Arc;

    use shiguredo_rtmp::{RtmpConnectionEvent, RtmpServerConnection};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Debug, Default)]
    pub(crate) struct Received {
        pub(crate) stream_names: Vec<String>,
        pub(crate) video: Vec<shiguredo_rtmp::VideoFrame>,
        pub(crate) audio: Vec<shiguredo_rtmp::AudioFrame>,
    }

    /// An in-process RTMP server: accepts publishes and records media.
    /// `drop_after_video` closes the first connection after that many video
    /// messages, to force a reconnect.
    pub(crate) async fn serve(
        listener: TcpListener,
        received: Arc<std::sync::Mutex<Received>>,
        drop_after_video: Option<usize>,
    ) {
        serve_with(listener, received, drop_after_video, None).await;
    }

    /// [`serve`], plus: once a publish is accepted, send a Set Peer Bandwidth
    /// of `peer_bandwidth` bytes while still acknowledging only every 2.5 MB
    /// (half the client's 5 MB window), the mismatch YouTube's ingest shows.
    pub(crate) async fn serve_with(
        listener: TcpListener,
        received: Arc<std::sync::Mutex<Received>>,
        drop_after_video: Option<usize>,
        peer_bandwidth: Option<u32>,
    ) {
        let mut first = true;
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let mut server = RtmpServerConnection::new();
            let mut buf = vec![0u8; 64 * 1024];
            let mut video_on_this_connection = 0usize;
            let mut send_peer_bandwidth = false;
            'conn: loop {
                while !server.send_buf().is_empty() {
                    let n = socket.write(server.send_buf()).await.unwrap_or(0);
                    if n == 0 {
                        break 'conn;
                    }
                    server.advance_send_buf(n);
                }
                // After a full drain, so it lands on a message boundary.
                if let Some(size) = peer_bandwidth.filter(|_| send_peer_bandwidth) {
                    send_peer_bandwidth = false;
                    if socket.write_all(&set_peer_bandwidth_message(size)).await.is_err() {
                        break;
                    }
                }
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                if server.feed_recv_buf(&buf[..n]).is_err() {
                    break;
                }
                while let Some(event) = server.next_event() {
                    let mut received = received.lock().unwrap();
                    match event {
                        RtmpConnectionEvent::PublishRequested { stream_name, .. } => {
                            received.stream_names.push(stream_name);
                            server.accept().unwrap();
                            send_peer_bandwidth = true;
                        }
                        RtmpConnectionEvent::VideoReceived(frame) => {
                            received.video.push(frame);
                            video_on_this_connection += 1;
                        }
                        RtmpConnectionEvent::AudioReceived(frame) => received.audio.push(frame),
                        _ => {}
                    }
                }
                if first && drop_after_video.is_some_and(|limit| video_on_this_connection >= limit) {
                    first = false;
                    break;
                }
            }
        }
    }

    /// One Set Peer Bandwidth message (type 6) on chunk stream 2, chunk
    /// format 0: window size (big-endian) and limit type 2 (dynamic).
    fn set_peer_bandwidth_message(size: u32) -> Vec<u8> {
        let mut message = vec![0x02, 0, 0, 0, 0, 0, 5, 6, 0, 0, 0, 0];
        message.extend_from_slice(&size.to_be_bytes());
        message.push(2);
        message
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::flv::{AacPackager, AvcPackager};
    use super::test_server::{serve, serve_with, Received};
    use shiguredo_rtmp::{AvcPacketType, AvcSequenceHeader, VideoCodec};
    use tokio::net::TcpListener;

    /// The go-live dialog's ready-made server URLs
    /// (src/lib/data/streamServices.ts) must all be publishable addresses.
    #[test]
    fn service_presets_parse_as_rtmps_destinations() {
        let source = include_str!("../../../src/lib/data/streamServices.ts");
        let urls: Vec<&str> = source
            .split("serverUrl: '")
            .skip(1)
            .filter_map(|rest| rest.split('\'').next())
            .filter(|url| !url.is_empty())
            .collect();
        assert!(urls.len() >= 4, "found {urls:?}");
        for url in urls {
            let destination = RtmpDestination::new(url, "live_key").unwrap_or_else(|e| panic!("{url}: {e}"));
            let parsed = destination.rtmp_url().unwrap_or_else(|_| panic!("{url} does not parse"));
            assert!(parsed.tls, "{url} should be RTMPS");
            assert_eq!(parsed.port, 443, "{url}");
            assert!(!parsed.app.is_empty() && !parsed.app.ends_with('/'), "{url}");
            assert_eq!(parsed.stream_name, "live_key");
        }
    }

    #[test]
    fn validates_destinations() {
        assert!(RtmpDestination::new("rtmps://live.example.com/app", "key").is_ok());
        assert!(RtmpDestination::new("RTMP://live.example.com:1935/live2/", "k").is_ok());
        assert_eq!(
            RtmpDestination::new("https://example.com/app", "k"),
            Err(DestinationError::Scheme)
        );
        assert_eq!(
            RtmpDestination::new("rtmp://user:pw@example.com/app", "k"),
            Err(DestinationError::Credentials)
        );
        assert_eq!(RtmpDestination::new("rtmp://example.com/app", " "), Err(DestinationError::MissingKey));
        let long_url = format!("rtmp://example.com/{}", "a".repeat(RTMP_URL_MAX_LEN));
        assert_eq!(RtmpDestination::new(&long_url, "k"), Err(DestinationError::UrlTooLong));
        assert_eq!(
            RtmpDestination::new("rtmp://example.com/app", &"k".repeat(RTMP_KEY_MAX_LEN + 1)),
            Err(DestinationError::KeyTooLong)
        );
        assert_eq!(RtmpDestination::new("rtmp://example.com", "k"), Err(DestinationError::Invalid));
    }

    #[test]
    fn a_server_url_with_the_key_in_it_is_refused() {
        let key = "abcd-efgh-ijkl-mnop";
        for url in [
            format!("rtmps://a.rtmps.youtube.com/live2/{key}"),
            format!("rtmp://live.example.com/app?key={key}"),
            format!("rtmp://live.example.com/app/prefix{key}"),
        ] {
            assert_eq!(RtmpDestination::new(&url, key), Err(DestinationError::KeyInUrl), "{url}");
        }
        // A short key only matches a whole segment or value.
        assert_eq!(
            RtmpDestination::new("rtmp://example.com/app/k1", "k1"),
            Err(DestinationError::KeyInUrl)
        );
        assert!(RtmpDestination::new("rtmp://example.com/live2", "live").is_ok());
        assert!(RtmpDestination::new("rtmp://k1.example.com/app", "k1").is_ok());
        assert_eq!(
            DestinationError::KeyInUrl.to_string(),
            "The server URL contains your stream key. Put the key only in the Stream key field."
        );
    }

    #[test]
    fn keys_with_url_characters_stay_intact_and_never_print() {
        let destination =
            RtmpDestination::new("rtmp://example.com/app", "live_123?auth=abc/def").unwrap();
        assert_eq!(destination.rtmp_url().unwrap().stream_name, "live_123?auth=abc/def");
        let debug = format!("{destination:?}");
        assert!(!debug.contains("live_123"), "{debug}");
        assert_eq!(destination.redact("bad name live_123?auth=abc/def"), "bad name [redacted]");
    }

    const SPS_A: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78];
    const SPS_B: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x50];
    const PPS: &[u8] = &[0x68, 0xeb, 0xe3, 0xcb];
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x00, 0x11];
    const P: &[u8] = &[0x41, 0x9a, 0x02, 0x03];

    async fn wait_for(received: &Arc<std::sync::Mutex<Received>>, check: impl Fn(&Received) -> bool) {
        for _ in 0..200 {
            if check(&received.lock().unwrap()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out: {:?}", received.lock().unwrap());
    }

    static TEST_DELAYS: [Duration; 3] =
        [Duration::from_millis(50), Duration::from_millis(50), Duration::from_millis(50)];

    #[tokio::test]
    async fn publishes_to_an_in_process_server_with_header_change_and_reconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(std::sync::Mutex::new(Received::default()));
        // Drop the first connection after: header + IDR + 2 P + header + IDR.
        let server = tokio::spawn(serve(listener, received.clone(), Some(6)));

        let destination =
            RtmpDestination::new(&format!("rtmp://127.0.0.1:{port}/live"), "secret-key").unwrap();
        let queue = Arc::new(MediaQueue::new());
        let stats = Arc::new(PublisherStats::default());
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(PublisherState::Connecting);
        let cancel = CancellationToken::new();
        let publisher = tokio::spawn(run_publisher(
            destination,
            queue.clone(),
            stats.clone(),
            state_tx,
            cancel.clone(),
            Some(&TEST_DELAYS),
        ));
        state_rx.wait_for(|state| *state == PublisherState::Live).await.unwrap();

        let mut avc = AvcPackager::new();
        let mut aac = AacPackager::default();
        let mut push_video = |nalus: &[&[u8]], ts| {
            for packet in avc.package(nalus, ts, false) {
                queue.push(packet);
            }
        };
        push_video(&[SPS_A, PPS, IDR], 0);
        for packet in aac.package(vec![0x21, 0x10], 0) {
            queue.push(packet);
        }
        push_video(&[P], 33);
        push_video(&[P], 66);
        // Follow the source: new SPS, new header, IDR.
        push_video(&[SPS_B, PPS, IDR], 100);
        wait_for(&received, |r| r.video.len() >= 6).await;
        {
            let r = received.lock().unwrap();
            assert_eq!(r.stream_names, vec!["secret-key".to_string()]);
            assert!(r.video.iter().all(|frame| frame.codec == VideoCodec::Avc));
            let headers: Vec<_> = r
                .video
                .iter()
                .filter(|frame| frame.avc_packet_type == Some(AvcPacketType::SequenceHeader))
                .map(|frame| AvcSequenceHeader::from_bytes(&frame.data).unwrap().sps_list[0].clone())
                .collect();
            assert_eq!(headers, vec![SPS_A.to_vec(), SPS_B.to_vec()]);
            // Orderly change: each header is immediately followed by a keyframe.
            for (index, frame) in r.video.iter().enumerate() {
                if frame.avc_packet_type == Some(AvcPacketType::SequenceHeader) {
                    assert!(r.video[index + 1].is_keyframe());
                }
            }
            assert!(r.audio.iter().any(|frame| frame.is_aac_sequence_header));
            let ts: Vec<u32> = r.video.iter().map(|f| f.timestamp.as_millis()).collect();
            assert!(ts.windows(2).all(|pair| pair[0] <= pair[1]), "{ts:?}");
        }

        // The server dropped us; the publisher reconnects.
        state_rx
            .wait_for(|state| matches!(state, PublisherState::Reconnecting { .. }))
            .await
            .unwrap();
        state_rx.wait_for(|state| *state == PublisherState::Live).await.unwrap();
        // Inter frames are dropped until a keyframe; the cached headers lead.
        assert!(queue.take_keyframe_request());
        push_video(&[P], 300);
        push_video(&[SPS_B, PPS, IDR], 333);
        push_video(&[P], 366);
        wait_for(&received, |r| r.stream_names.len() == 2 && r.video.len() >= 9).await;
        {
            let r = received.lock().unwrap();
            let resumed = &r.video[6..];
            assert_eq!(resumed[0].avc_packet_type, Some(AvcPacketType::SequenceHeader));
            assert_eq!(resumed[0].timestamp.as_millis(), 0);
            assert!(resumed[1].is_keyframe(), "resumes on a keyframe");
            assert_eq!(resumed[1].timestamp.as_millis(), 0, "timestamps rebased per connection");
            assert_eq!(resumed[2].timestamp.as_millis(), 33);
            assert!(r.audio.iter().filter(|f| f.is_aac_sequence_header).count() >= 2);
        }
        assert!(stats.bytes_sent.load(Ordering::Relaxed) > 0);

        cancel.cancel();
        assert_eq!(publisher.await.unwrap(), PublisherState::Stopped);
        server.abort();
    }

    /// YouTube advertises a Set Peer Bandwidth smaller than the interval it
    /// acknowledges at. The publisher must keep publishing rather than treat
    /// the "late" acknowledgements as a dead connection (logs/001.log: "ACK
    /// not received within expected interval" every 5-100 s).
    #[tokio::test]
    async fn keeps_publishing_when_the_server_acknowledges_less_often_than_it_advertised() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(std::sync::Mutex::new(Received::default()));
        let server = tokio::spawn(serve_with(listener, received.clone(), None, Some(4096)));
        let destination = RtmpDestination::new(&format!("rtmp://127.0.0.1:{port}/live"), "k").unwrap();
        let queue = Arc::new(MediaQueue::new());
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(PublisherState::Connecting);
        let cancel = CancellationToken::new();
        let publisher = tokio::spawn(run_publisher(
            destination,
            queue.clone(),
            Arc::new(PublisherStats::default()),
            state_tx,
            cancel.clone(),
            Some(&TEST_DELAYS),
        ));
        state_rx.wait_for(|state| *state == PublisherState::Live).await.unwrap();

        // ~160 KB: far past twice the advertised 4 KB, far short of the
        // 2.5 MB the server acknowledges at.
        let mut avc = AvcPackager::new();
        let slice = [&[0x41u8][..], &[0x9a; 4000][..]].concat();
        for packet in avc.package(&[SPS_A, PPS, IDR], 0, false) {
            queue.push(packet);
        }
        for frame in 1..=40u32 {
            for packet in avc.package(&[&slice], frame * 33, false) {
                queue.push(packet);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        wait_for(&received, |r| r.video.len() >= 42).await;
        assert_eq!(*state_rx.borrow(), PublisherState::Live);
        assert_eq!(received.lock().unwrap().stream_names.len(), 1, "never reconnected");

        cancel.cancel();
        assert_eq!(publisher.await.unwrap(), PublisherState::Stopped);
        server.abort();
    }

    #[tokio::test]
    async fn a_first_connection_failure_is_final() {
        // Bind then drop: nothing listens on this port.
        let port = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
        let destination =
            RtmpDestination::new(&format!("rtmp://127.0.0.1:{port}/live"), "k").unwrap();
        let (state_tx, _state_rx) = tokio::sync::watch::channel(PublisherState::Connecting);
        let state = run_publisher(
            destination,
            Arc::new(MediaQueue::new()),
            Arc::new(PublisherStats::default()),
            state_tx,
            CancellationToken::new(),
            Some(&TEST_DELAYS),
        )
        .await;
        assert!(matches!(state, PublisherState::Failed(_)), "{state:?}");
    }
}
