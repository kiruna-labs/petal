//! This computer's own live-stream participant attributes (`petal.live`,
//! `petal.stream-optout`): what they should be, and a writer that keeps
//! writing until the server takes them.
//!
//! A failed `set_attributes` is retried with backoff, never dropped: a LIVE
//! indicator nobody sees, or an opt-out the streamer never reads, is a privacy
//! failure, not a cosmetic one. Every `Reconnected` (a resume or a full
//! reconnect) and every local attribute change that loses a written value
//! writes them all again: a full reconnect rejoins with no attributes.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::{watch, Notify};
use tokio_util::sync::CancellationToken;

use crate::sync_ext::MutexExt;

/// First retry delay after a failed write; doubles up to [`RETRY_MAX`].
pub const RETRY_MIN: Duration = Duration::from_millis(250);
pub const RETRY_MAX: Duration = Duration::from_secs(5);

pub type WriteFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Where the attributes go: the room's local participant in the app, a fake
/// in tests. An empty value deletes the key.
pub trait AttributeSink: Send + Sync + 'static {
    fn write(&self, attributes: HashMap<String, String>) -> WriteFuture<'_>;
}

/// The joined room's local participant.
pub struct RoomSink(pub Weak<livekit::Room>);

impl AttributeSink for RoomSink {
    fn write(&self, attributes: HashMap<String, String>) -> WriteFuture<'_> {
        Box::pin(async move {
            let room = self.0.upgrade().ok_or_else(|| "the room is gone".to_string())?;
            room.local_participant().set_attributes(attributes).await.map_err(|e| e.to_string())
        })
    }
}

/// One pending write: key, value, and the revision it answers.
type Batch = Vec<(String, String, u64)>;

#[derive(Debug, Default)]
struct State {
    /// What each key should be ("" = absent).
    desired: HashMap<String, String>,
    /// What the server holds, as far as we know. A key not in the map was
    /// never written on this join (absent, so ""); `None` is unknown: a write
    /// that failed or timed out (LiveKit can apply a request whose response
    /// it then misses) or a reconnect since the last write.
    confirmed: HashMap<String, Option<String>>,
    /// Bumped whenever a key must be written again; a write settles a key
    /// only if no newer request came in while it was in flight.
    revision: HashMap<String, u64>,
    /// Keys still to write.
    dirty: BTreeSet<String>,
    /// Keys with a write in flight.
    inflight: BTreeSet<String>,
}

impl State {
    fn known(&self, key: &str) -> Option<&str> {
        match self.confirmed.get(key) {
            None => Some(""),
            Some(value) => value.as_deref(),
        }
    }

    fn mark_dirty(&mut self, key: &str) {
        *self.revision.entry(key.to_string()).or_default() += 1;
        self.dirty.insert(key.to_string());
    }
}

#[derive(Debug)]
pub struct AttributeKeeper {
    state: Mutex<State>,
    wake: Notify,
    /// Bumped whenever a write settles, for [`AttributeKeeper::wait_confirmed`].
    written: watch::Sender<u64>,
}

impl Default for AttributeKeeper {
    fn default() -> Self {
        Self { state: Mutex::default(), wake: Notify::new(), written: watch::channel(0).0 }
    }
}

impl AttributeKeeper {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// `key` should be `value` ("" deletes it). Written by [`Self::run`].
    pub fn set(&self, key: &str, value: &str) {
        let mut state = self.state.lock_unpoisoned();
        state.desired.insert(key.to_string(), value.to_string());
        // Already there (an absent key is "" on a fresh join), with nothing
        // in flight that could change it: nothing to write.
        if !state.inflight.contains(key) && state.known(key) == Some(value) {
            state.dirty.remove(key);
            return;
        }
        state.mark_dirty(key);
        drop(state);
        self.wake.notify_one();
    }

    /// The value the server holds for `key`, as far as we know: "" when
    /// absent, `None` while unknown (a write pending after a failure or a
    /// reconnect).
    pub fn confirmed(&self, key: &str) -> Option<String> {
        self.state.lock_unpoisoned().known(key).map(str::to_string)
    }

    /// Write every set value again: after a reconnect, which may have been a
    /// full one that rejoined with no attributes. Until rewritten, none of
    /// them counts as confirmed.
    pub fn reassert_all(&self) {
        let mut state = self.state.lock_unpoisoned();
        let keys: Vec<String> = state
            .desired
            .iter()
            .filter(|(key, value)| !value.is_empty() || state.known(key).is_none())
            .map(|(key, _)| key.clone())
            .collect();
        if keys.is_empty() {
            return;
        }
        for key in keys {
            state.confirmed.insert(key.clone(), None);
            state.mark_dirty(&key);
        }
        drop(state);
        self.wake.notify_one();
    }

    /// The room now shows `current` for our own attributes. A value we wrote
    /// that isn't there any more (a full reconnect rejoined without it) is
    /// written again.
    pub fn observe_local(&self, current: &HashMap<String, String>) {
        let mut state = self.state.lock_unpoisoned();
        let lost: Vec<String> = state
            .desired
            .iter()
            .filter(|(key, value)| {
                !value.is_empty()
                    && !state.inflight.contains(*key)
                    && state.known(key) == Some(value.as_str())
                    && current.get(*key) != Some(*value)
            })
            .map(|(key, _)| key.clone())
            .collect();
        if lost.is_empty() {
            return;
        }
        for key in lost {
            log::info!("streaming: the room lost our {key} attribute; writing it again");
            state.confirmed.insert(key.clone(), None);
            state.mark_dirty(&key);
        }
        drop(state);
        self.wake.notify_one();
    }

    /// Wait until the server holds `value` for `key`, up to `timeout`.
    pub async fn wait_confirmed(&self, key: &str, value: &str, timeout: Duration) -> bool {
        let mut written = self.written.subscribe();
        let wait = async {
            loop {
                if self.confirmed(key).as_deref() == Some(value) {
                    return;
                }
                if written.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        };
        tokio::time::timeout(timeout, wait).await.is_ok()
    }

    /// The pending writes, or `None` when everything is written.
    fn next_batch(&self) -> Option<Batch> {
        let mut state = self.state.lock_unpoisoned();
        if state.dirty.is_empty() {
            return None;
        }
        let batch: Batch = state
            .dirty
            .iter()
            .map(|key| {
                let value = state.desired.get(key).cloned().unwrap_or_default();
                (key.clone(), value, state.revision.get(key).copied().unwrap_or_default())
            })
            .collect();
        for (key, _, _) in &batch {
            state.inflight.insert(key.clone());
        }
        Some(batch)
    }

    /// The server accepted `batch`.
    fn accept(&self, batch: Batch) {
        let mut state = self.state.lock_unpoisoned();
        for (key, value, revision) in batch {
            state.inflight.remove(&key);
            if state.revision.get(&key).copied().unwrap_or_default() != revision {
                // A newer request (a change, or a reconnect) came in while
                // this was in flight: it stays dirty and decides the value.
                state.confirmed.insert(key, None);
                continue;
            }
            state.confirmed.insert(key.clone(), Some(value.clone()));
            if state.desired.get(&key) == Some(&value) {
                state.dirty.remove(&key);
            } else {
                state.mark_dirty(&key);
            }
        }
        drop(state);
        self.written.send_modify(|count| *count = count.wrapping_add(1));
    }

    /// `batch` failed or timed out: it may or may not have reached the
    /// server, so what it holds is unknown until a write succeeds.
    fn reject(&self, batch: &Batch) {
        let mut state = self.state.lock_unpoisoned();
        for (key, _, _) in batch {
            state.inflight.remove(key);
            state.confirmed.insert(key.clone(), None);
        }
        drop(state);
        self.written.send_modify(|count| *count = count.wrapping_add(1));
    }

    /// Write until `cancel` (the room ended): each change as it is set, and a
    /// failed write again after a backoff, until it sticks.
    pub async fn run(self: Arc<Self>, sink: impl AttributeSink, cancel: CancellationToken) {
        let mut delay = RETRY_MIN;
        let mut failures = 0u32;
        loop {
            let Some(batch) = self.next_batch() else {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = self.wake.notified() => continue,
                }
            };
            let keys: Vec<&String> = batch.iter().map(|(key, _, _)| key).collect();
            let keys = format!("{keys:?}");
            let attributes = batch.iter().map(|(key, value, _)| (key.clone(), value.clone())).collect();
            let result = tokio::select! {
                () = cancel.cancelled() => return,
                result = sink.write(attributes) => result,
            };
            match result {
                Ok(()) => {
                    if failures > 0 {
                        log::info!("streaming: wrote {keys} after {failures} failed attempt(s)");
                    }
                    failures = 0;
                    delay = RETRY_MIN;
                    self.accept(batch);
                }
                Err(error) => {
                    self.reject(&batch);
                    failures += 1;
                    // Warn on the first failure and then every 10th, so a long
                    // outage stays visible without flooding the log.
                    if failures == 1 || failures % 10 == 0 {
                        log::warn!(
                            "streaming: couldn't write {keys} (attempt {failures}, retrying in {delay:?}): {error}"
                        );
                    }
                    tokio::select! {
                        () = cancel.cancelled() => return,
                        () = tokio::time::sleep(delay) => {}
                    }
                    delay = (delay * 2).min(RETRY_MAX);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A participant's attributes on the server, failing the first `fail`
    /// writes the way a timed-out signal request does.
    #[derive(Default)]
    struct FakeRoom {
        attributes: Mutex<HashMap<String, String>>,
        /// Writes to refuse outright.
        fail: AtomicU32,
        /// Writes to apply and then report as timed out.
        apply_then_fail: AtomicU32,
        /// How long the response to an applied write takes.
        response_delay: Mutex<Duration>,
        writes: AtomicU32,
    }

    impl FakeRoom {
        fn get(&self, key: &str) -> Option<String> {
            self.attributes.lock_unpoisoned().get(key).cloned()
        }

        /// A full reconnect: the server knows nothing we wrote.
        fn rejoin(&self) {
            self.attributes.lock_unpoisoned().clear();
        }
    }

    struct Sink(Arc<FakeRoom>);

    impl AttributeSink for Sink {
        fn write(&self, attributes: HashMap<String, String>) -> WriteFuture<'_> {
            Box::pin(async move {
                self.0.writes.fetch_add(1, Ordering::SeqCst);
                if self.0.fail.load(Ordering::SeqCst) > 0 {
                    self.0.fail.fetch_sub(1, Ordering::SeqCst);
                    return Err("signal request timeout".into());
                }
                {
                    let mut stored = self.0.attributes.lock_unpoisoned();
                    for (key, value) in attributes {
                        if value.is_empty() {
                            stored.remove(&key);
                        } else {
                            stored.insert(key, value);
                        }
                    }
                }
                let delay = *self.0.response_delay.lock_unpoisoned();
                tokio::time::sleep(delay).await;
                if self.0.apply_then_fail.load(Ordering::SeqCst) > 0 {
                    self.0.apply_then_fail.fetch_sub(1, Ordering::SeqCst);
                    return Err("request timeout".into());
                }
                Ok(())
            })
        }
    }

    fn start(room: &Arc<FakeRoom>) -> (Arc<AttributeKeeper>, CancellationToken) {
        let keeper = AttributeKeeper::new();
        let cancel = CancellationToken::new();
        tokio::spawn(keeper.clone().run(Sink(room.clone()), cancel.clone()));
        (keeper, cancel)
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_live_attribute_write_is_retried_until_it_sticks() {
        let room = Arc::new(FakeRoom::default());
        room.fail.store(4, Ordering::SeqCst);
        let (keeper, _cancel) = start(&room);
        keeper.set("petal.live", "{\"state\":\"live\"}");
        // Four failures back off 250 ms, 500 ms, 1 s, 2 s; the fifth sticks.
        assert!(keeper.wait_confirmed("petal.live", "{\"state\":\"live\"}", Duration::from_secs(10)).await);
        assert_eq!(room.get("petal.live").as_deref(), Some("{\"state\":\"live\"}"));
        assert_eq!(room.writes.load(Ordering::SeqCst), 5);
    }

    #[tokio::test(start_paused = true)]
    async fn the_retry_backoff_is_capped_and_never_gives_up() {
        let room = Arc::new(FakeRoom::default());
        room.fail.store(30, Ordering::SeqCst);
        let (keeper, _cancel) = start(&room);
        keeper.set("petal.stream-optout", "1");
        // 0.25+0.5+1+2 s, then 26 more failures at the 5 s cap: ~134 s.
        assert!(!keeper.wait_confirmed("petal.stream-optout", "1", Duration::from_secs(120)).await);
        assert!(keeper.wait_confirmed("petal.stream-optout", "1", Duration::from_secs(30)).await);
        assert_eq!(room.get("petal.stream-optout").as_deref(), Some("1"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_reconnect_writes_both_attributes_again() {
        let room = Arc::new(FakeRoom::default());
        let (keeper, _cancel) = start(&room);
        keeper.set("petal.live", "live");
        keeper.set("petal.stream-optout", "1");
        assert!(keeper.wait_confirmed("petal.live", "live", Duration::from_secs(1)).await);
        assert!(keeper.wait_confirmed("petal.stream-optout", "1", Duration::from_secs(1)).await);
        // The rejoin drops both; the Reconnected that follows writes them back.
        room.rejoin();
        keeper.reassert_all();
        assert_eq!(keeper.confirmed("petal.live"), None, "nothing counts as announced until rewritten");
        assert!(keeper.wait_confirmed("petal.live", "live", Duration::from_secs(1)).await);
        assert!(keeper.wait_confirmed("petal.stream-optout", "1", Duration::from_secs(1)).await);
        assert_eq!(room.get("petal.live").as_deref(), Some("live"));
        assert_eq!(room.get("petal.stream-optout").as_deref(), Some("1"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_attribute_seen_on_the_local_participant_is_written_again() {
        let room = Arc::new(FakeRoom::default());
        let (keeper, _cancel) = start(&room);
        keeper.set("petal.stream-optout", "1");
        assert!(keeper.wait_confirmed("petal.stream-optout", "1", Duration::from_secs(1)).await);
        // The rejoined participant's info arrives without it.
        room.rejoin();
        keeper.observe_local(&HashMap::new());
        assert!(keeper.wait_confirmed("petal.stream-optout", "1", Duration::from_secs(1)).await);
        assert_eq!(room.get("petal.stream-optout").as_deref(), Some("1"));
        // Still there: no extra write.
        let writes = room.writes.load(Ordering::SeqCst);
        keeper.observe_local(&HashMap::from([("petal.stream-optout".to_string(), "1".to_string())]));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(room.writes.load(Ordering::SeqCst), writes);
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_write_that_did_apply_is_still_cleared_on_stop() {
        let room = Arc::new(FakeRoom::default());
        // The server takes petal.live, but the response never comes back.
        room.apply_then_fail.store(1, Ordering::SeqCst);
        let (keeper, _cancel) = start(&room);
        keeper.set("petal.live", "live");
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(room.get("petal.live").as_deref(), Some("live"));
        assert_eq!(keeper.confirmed("petal.live"), None, "a timed-out write leaves the value unknown");
        // Stopping clears it, and that clear is really written.
        keeper.set("petal.live", "");
        assert!(keeper.wait_confirmed("petal.live", "", Duration::from_secs(5)).await);
        assert_eq!(room.get("petal.live"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_write_answered_after_a_full_reconnect_does_not_cancel_the_rewrite() {
        let room = Arc::new(FakeRoom::default());
        *room.response_delay.lock_unpoisoned() = Duration::from_millis(500);
        let (keeper, _cancel) = start(&room);
        keeper.set("petal.live", "live");
        // The write reached the old session; before its answer arrives the
        // room fully reconnects and loses it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        room.rejoin();
        keeper.reassert_all();
        // The late answer must not count as the rewrite.
        assert!(keeper.wait_confirmed("petal.live", "live", Duration::from_secs(5)).await);
        assert_eq!(room.get("petal.live").as_deref(), Some("live"));
        assert!(room.writes.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_value_set_back_while_a_write_is_in_flight_is_written_again() {
        let room = Arc::new(FakeRoom::default());
        let (keeper, _cancel) = start(&room);
        keeper.set("petal.live", "live");
        assert!(keeper.wait_confirmed("petal.live", "live", Duration::from_secs(1)).await);
        // "paused" goes out; before it lands, the stream is live again.
        keeper.set("petal.live", "paused");
        let batch = keeper.next_batch().unwrap();
        keeper.set("petal.live", "live");
        keeper.accept(batch);
        assert!(keeper.wait_confirmed("petal.live", "live", Duration::from_secs(1)).await);
        assert_eq!(room.get("petal.live").as_deref(), Some("live"));
    }

    #[tokio::test(start_paused = true)]
    async fn the_latest_value_wins_and_a_clear_is_written_too() {
        let room = Arc::new(FakeRoom::default());
        room.fail.store(1, Ordering::SeqCst);
        let (keeper, _cancel) = start(&room);
        keeper.set("petal.live", "live");
        keeper.set("petal.live", "paused");
        assert!(keeper.wait_confirmed("petal.live", "paused", Duration::from_secs(2)).await);
        keeper.set("petal.live", "");
        assert!(keeper.wait_confirmed("petal.live", "", Duration::from_secs(2)).await);
        assert_eq!(room.get("petal.live"), None);
        // Opting back in on a fresh join writes nothing: absent is included.
        let writes = room.writes.load(Ordering::SeqCst);
        keeper.set("petal.stream-optout", "");
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(room.writes.load(Ordering::SeqCst), writes);
    }
}
