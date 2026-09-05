//! #915: a pure, generic-over-effects async driver for refreshing a shared
//! browser window's URL after share start, without blocking the share-start
//! path and without leaking a background task past its share's lifetime.
//!
//! `share.rs`'s `spawn_share_url_refresh` is the real macOS caller: it wires
//! `extract` to `browser_url::extract_url_for_window` (via
//! `spawn_blocking`, since the underlying `osascript` call is a blocking
//! child-process wait -- see CLAUDE.md's crash-class #3), `current_title` to
//! the window's title, and `sink` to
//! `RoomConnection::set_shared_window_url_for_generation` under the shared
//! metadata-apply lock. Everything here is deliberately effect-free (no
//! `RoomConnection`, no `ActiveShare`, no real capture) so the scheduling
//! policy itself -- when to attempt, when to skip, when to give up -- is
//! directly unit-testable under `tokio::time`'s paused clock, the same way
//! `share.rs`'s own doc comment says a narrow seam should be (`share.rs:2140`
//! notes `ActiveShare` cannot be faked).

use std::future::Future;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::browser_url::UrlExtraction;

/// Timing policy for one `run_url_refresh` run. See `browser_url`'s
/// `FIRST_ATTEMPT_TIMEOUT`/`POLL_TIMEOUT`/`POLL_INTERVAL`/`FRESH_URL_TTL`
/// constants for the real production values (issue #915's plan: 60s first
/// attempt so a pending Automation consent prompt or a cold browser isn't
/// killed mid-prompt; 3s timeout/interval thereafter; skip an extraction
/// when the title hasn't changed and the last success is under 15s old).
#[derive(Debug, Clone, Copy)]
pub struct UrlRefreshConfig {
    pub first_attempt_timeout: std::time::Duration,
    pub poll_timeout: std::time::Duration,
    pub poll_interval: std::time::Duration,
    pub fresh_url_ttl: std::time::Duration,
}

/// One extraction attempt's parameters. Just the timeout today, but a
/// distinct type (rather than a bare `Duration`) keeps the `extract`
/// closure's signature self-documenting at call sites and leaves room to
/// carry more per-attempt context later without breaking every caller.
#[derive(Debug, Clone, Copy)]
pub struct Attempt {
    pub timeout: std::time::Duration,
}

/// Why `run_url_refresh` returned. There is no "ran forever" variant because
/// the function never returns for that case -- the caller's `JoinHandle` is
/// what bounds an otherwise-endless run (aborted on every `ActiveShare`
/// teardown path; see `share.rs`'s `ActiveShare::url_refresh` doc comment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshExit {
    /// `cancel` was signalled (or already signalled at entry).
    Cancelled,
    /// `extract` returned a terminal outcome (`UrlExtraction::is_terminal`,
    /// i.e. `Denied` or `Unsupported`) -- retrying can never succeed for the
    /// remainder of this process's life, so the loop gives up for good.
    Terminal,
}

/// Run the refresh loop until cancelled or a terminal extraction outcome.
///
/// - `extract(attempt)` performs one extraction attempt bounded by
///   `attempt.timeout` and returns its outcome. The FIRST call in a run uses
///   `cfg.first_attempt_timeout`; every call after that uses
///   `cfg.poll_timeout`.
/// - `current_title()` returns the shared window's current title, if known.
///   Between polls (never on the first attempt), if the title returned here
///   is unchanged from the title at the last SUCCESSFUL extraction and that
///   success happened less than `cfg.fresh_url_ttl` ago, this tick's
///   extraction is skipped entirely (no `extract` call at all) -- the URL
///   can't plausibly have changed without the tab/window retitling.
/// - `sink(Some(url))` is called exactly when a successful extraction's URL
///   differs from the last URL this run sent (including the very first
///   success). It is NEVER called with `None` -- a transient failure
///   (`Empty`/`Ambiguous`/`Timeout`/`Failed`/`Spawn`) holds the last good
///   value rather than clearing it (same spirit as the product's
///   never-black-frame rule: freeze, don't blank, on a gap) -- and the
///   share-start path is what publishes the initial `None`, not this driver.
///   `sink` returns whether the write actually landed (`true` = staged and
///   published); the driver only records the URL as sent when it does. A
///   `false` (e.g. a generation mismatch, or a failed `set_metadata` call)
///   leaves the URL eligible to be sent again on a later successful
///   extraction of the same URL, rather than being silently dropped.
/// - `on_failure(outcome, first_for_share)` is called for every non-`Url`
///   outcome (including terminal ones), with `first_for_share` true for
///   exactly the first such call in this run and false for every call after.
/// - `cancel` is checked before every attempt (including the first) and
///   raced against both the inter-poll sleep and an in-flight `extract`
///   call, so cancellation is prompt even if `extract` is slow to resolve.
pub async fn run_url_refresh<Ex, ExFut, T, Sk, SkFut>(
    cfg: UrlRefreshConfig,
    mut extract: Ex,
    mut current_title: T,
    mut sink: Sk,
    cancel: CancellationToken,
    mut on_failure: impl FnMut(&UrlExtraction, bool),
) -> RefreshExit
where
    Ex: FnMut(Attempt) -> ExFut,
    ExFut: Future<Output = UrlExtraction>,
    T: FnMut() -> Option<String>,
    Sk: FnMut(Option<String>) -> SkFut,
    SkFut: Future<Output = bool>,
{
    let mut last_sent: Option<String> = None;
    // (title at the last successful extraction, when that success happened)
    let mut last_success: Option<(Option<String>, Instant)> = None;
    let mut first_failure = true;
    let mut first_attempt = true;

    loop {
        if cancel.is_cancelled() {
            return RefreshExit::Cancelled;
        }

        if !first_attempt {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return RefreshExit::Cancelled,
                _ = tokio::time::sleep(cfg.poll_interval) => {}
            }
            if cancel.is_cancelled() {
                return RefreshExit::Cancelled;
            }
        }

        let title_now = current_title();
        // Only ever true after at least one success, so this never skips
        // the unconditional first attempt.
        if let Some((success_title, success_at)) = &last_success {
            if *success_title == title_now && success_at.elapsed() < cfg.fresh_url_ttl {
                continue;
            }
        }

        let timeout = if first_attempt {
            cfg.first_attempt_timeout
        } else {
            cfg.poll_timeout
        };
        first_attempt = false;

        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => return RefreshExit::Cancelled,
            outcome = extract(Attempt { timeout }) => outcome,
        };

        if let Some(url) = outcome.url() {
            if last_sent.as_deref() != Some(url) {
                let url = url.to_string();
                if sink(Some(url.clone())).await {
                    last_sent = Some(url);
                }
            }
            last_success = Some((title_now, Instant::now()));
            continue;
        }

        on_failure(&outcome, first_failure);
        first_failure = false;
        if outcome.is_terminal() {
            return RefreshExit::Terminal;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn test_cfg() -> UrlRefreshConfig {
        UrlRefreshConfig {
            first_attempt_timeout: std::time::Duration::from_secs(60),
            poll_timeout: std::time::Duration::from_secs(3),
            poll_interval: std::time::Duration::from_secs(3),
            fresh_url_ttl: std::time::Duration::from_secs(15),
        }
    }

    /// Queue-driven extractor: pops one scripted `UrlExtraction` per call,
    /// repeating the last entry once the queue is drained (so a test can
    /// script N attempts precisely and let the run continue past them
    /// without panicking on an empty queue).
    struct ScriptedExtractor {
        remaining: Mutex<Vec<UrlExtraction>>,
        calls: AtomicUsize,
    }

    impl ScriptedExtractor {
        fn new(outcomes: Vec<UrlExtraction>) -> Self {
            Self {
                remaining: Mutex::new(outcomes),
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        async fn next(&self) -> UrlExtraction {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut remaining = self.remaining.lock().unwrap();
            if remaining.len() > 1 {
                remaining.remove(0)
            } else {
                remaining
                    .last()
                    .cloned()
                    .unwrap_or(UrlExtraction::Empty)
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn empty_then_url_sends_exactly_one_update() {
        let extractor = std::sync::Arc::new(ScriptedExtractor::new(vec![
            UrlExtraction::Empty,
            UrlExtraction::Url("https://example.com/a".to_string()),
        ]));
        let sink_calls = std::sync::Arc::new(Mutex::new(Vec::new()));
        let failure_calls = std::sync::Arc::new(Mutex::new(Vec::new()));

        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let handle = {
            let extractor = extractor.clone();
            let sink_calls = sink_calls.clone();
            let failure_calls = failure_calls.clone();
            tokio::spawn(async move {
                run_url_refresh(
                    test_cfg(),
                    move |_attempt| {
                        let extractor = extractor.clone();
                        async move { extractor.next().await }
                    },
                    || None,
                    move |url| {
                        let sink_calls = sink_calls.clone();
                        async move { sink_calls.lock().unwrap().push(url); true }
                    },
                    task_cancel,
                    move |outcome, first| failure_calls.lock().unwrap().push((outcome.clone(), first)),
                )
                .await
            })
        };

        // First attempt (Empty) runs immediately; advance past the poll
        // interval so the second attempt (Url) fires, then stop the run.
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        cancel.cancel();
        let exit = handle.await.unwrap();
        assert_eq!(exit, RefreshExit::Cancelled);

        assert_eq!(extractor.calls(), 2, "expected exactly two extraction attempts");
        let sent = sink_calls.lock().unwrap().clone();
        assert_eq!(
            sent,
            vec![Some("https://example.com/a".to_string())],
            "exactly one sink call, carrying the successful URL"
        );
        let failures = failure_calls.lock().unwrap().clone();
        assert_eq!(failures.len(), 1, "one failure recorded for the Empty attempt");
        assert!(failures[0].1, "the sole failure must be flagged first_for_share");
    }

    #[tokio::test(start_paused = true)]
    async fn identical_url_twice_sends_one_update() {
        let extractor = std::sync::Arc::new(ScriptedExtractor::new(vec![
            UrlExtraction::Url("https://example.com/same".to_string()),
        ]));
        let sink_calls = std::sync::Arc::new(Mutex::new(Vec::new()));

        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let handle = {
            let extractor = extractor.clone();
            let sink_calls = sink_calls.clone();
            tokio::spawn(async move {
                run_url_refresh(
                    test_cfg(),
                    move |_attempt| {
                        let extractor = extractor.clone();
                        async move { extractor.next().await }
                    },
                    // Title changes every poll so the freshness skip never
                    // engages -- this test is about de-duplicating an
                    // unchanged URL, not about the skip path.
                    {
                        let mut n = 0u32;
                        move || {
                            n += 1;
                            Some(format!("title-{n}"))
                        }
                    },
                    move |url| {
                        let sink_calls = sink_calls.clone();
                        async move { sink_calls.lock().unwrap().push(url); true }
                    },
                    task_cancel,
                    |_outcome, _first| {},
                )
                .await
            })
        };

        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        cancel.cancel();
        let exit = handle.await.unwrap();
        assert_eq!(exit, RefreshExit::Cancelled);

        let sent = sink_calls.lock().unwrap().clone();
        assert_eq!(
            sent,
            vec![Some("https://example.com/same".to_string())],
            "the second identical URL must not re-trigger the sink"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_stops_the_loop_promptly() {
        let extractor = std::sync::Arc::new(ScriptedExtractor::new(vec![UrlExtraction::Empty]));
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let handle = {
            let extractor = extractor.clone();
            tokio::spawn(async move {
                run_url_refresh(
                    test_cfg(),
                    move |_attempt| {
                        let extractor = extractor.clone();
                        async move { extractor.next().await }
                    },
                    || None,
                    |_url| async { true },
                    task_cancel,
                    |_outcome, _first| {},
                )
                .await
            })
        };

        tokio::task::yield_now().await;
        cancel.cancel();
        let exit = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("run_url_refresh did not stop promptly after cancel")
            .unwrap();
        assert_eq!(exit, RefreshExit::Cancelled);
    }

    #[tokio::test(start_paused = true)]
    async fn denied_stops_with_terminal_and_no_further_attempts() {
        let extractor = std::sync::Arc::new(ScriptedExtractor::new(vec![UrlExtraction::Denied]));
        let cancel = CancellationToken::new();
        let handle = {
            let extractor = extractor.clone();
            tokio::spawn(async move {
                run_url_refresh(
                    test_cfg(),
                    move |_attempt| {
                        let extractor = extractor.clone();
                        async move { extractor.next().await }
                    },
                    || None,
                    |_url| async { true },
                    cancel,
                    |_outcome, _first| {},
                )
                .await
            })
        };

        let exit = handle.await.unwrap();
        assert_eq!(exit, RefreshExit::Terminal);
        // Advancing time (were the loop still alive, it would poll again)
        // is moot once the task has already exited -- `calls()` below
        // pins the count at exactly one regardless.
        assert_eq!(extractor.calls(), 1, "Denied must not be retried");
    }

    #[tokio::test(start_paused = true)]
    async fn unchanged_fresh_title_skips_extraction_changed_title_triggers_it() {
        let extractor = std::sync::Arc::new(ScriptedExtractor::new(vec![
            UrlExtraction::Url("https://example.com/x".to_string()),
        ]));
        let titles = std::sync::Arc::new(Mutex::new(vec![
            Some("Tab A".to_string()), // first attempt
            Some("Tab A".to_string()), // poll 1: unchanged -> should skip
            Some("Tab B".to_string()), // poll 2: changed -> should extract
        ]));

        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let handle = {
            let extractor = extractor.clone();
            let titles = titles.clone();
            tokio::spawn(async move {
                run_url_refresh(
                    test_cfg(),
                    move |_attempt| {
                        let extractor = extractor.clone();
                        async move { extractor.next().await }
                    },
                    move || {
                        let mut titles = titles.lock().unwrap();
                        if titles.len() > 1 {
                            titles.remove(0)
                        } else {
                            titles.last().cloned().flatten()
                        }
                    },
                    |_url| async { true },
                    task_cancel,
                    |_outcome, _first| {},
                )
                .await
            })
        };

        // First attempt: extracts (1 call).
        tokio::task::yield_now().await;
        // Poll 1 (title unchanged, success fresh): must skip -- still 1 call.
        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        assert_eq!(extractor.calls(), 1, "unchanged fresh title must skip extraction");
        // Poll 2 (title changed): must extract -- 2 calls.
        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        assert_eq!(extractor.calls(), 2, "a changed title must trigger extraction");

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn transient_timeout_after_success_does_not_clear_the_sunk_url() {
        let extractor = std::sync::Arc::new(ScriptedExtractor::new(vec![
            UrlExtraction::Url("https://example.com/held".to_string()),
            UrlExtraction::Timeout,
        ]));
        let sink_calls = std::sync::Arc::new(Mutex::new(Vec::new()));

        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let handle = {
            let extractor = extractor.clone();
            let sink_calls = sink_calls.clone();
            tokio::spawn(async move {
                run_url_refresh(
                    test_cfg(),
                    move |_attempt| {
                        let extractor = extractor.clone();
                        async move { extractor.next().await }
                    },
                    {
                        // Force past the freshness skip so the Timeout
                        // attempt actually happens on the next poll.
                        let mut n = 0u32;
                        move || {
                            n += 1;
                            Some(format!("title-{n}"))
                        }
                    },
                    move |url| {
                        let sink_calls = sink_calls.clone();
                        async move { sink_calls.lock().unwrap().push(url); true }
                    },
                    task_cancel,
                    |_outcome, _first| {},
                )
                .await
            })
        };

        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        cancel.cancel();
        handle.await.unwrap();

        let sent = sink_calls.lock().unwrap().clone();
        assert_eq!(
            sent,
            vec![Some("https://example.com/held".to_string())],
            "a later transient Timeout must never emit sink(None)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn first_failure_flag_is_true_exactly_once() {
        let extractor = std::sync::Arc::new(ScriptedExtractor::new(vec![
            UrlExtraction::Empty,
            UrlExtraction::Timeout,
            UrlExtraction::Empty,
        ]));
        let failure_flags = std::sync::Arc::new(Mutex::new(Vec::new()));

        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let handle = {
            let extractor = extractor.clone();
            let failure_flags = failure_flags.clone();
            tokio::spawn(async move {
                run_url_refresh(
                    test_cfg(),
                    move |_attempt| {
                        let extractor = extractor.clone();
                        async move { extractor.next().await }
                    },
                    || None,
                    |_url| async { true },
                    task_cancel,
                    move |_outcome, first| failure_flags.lock().unwrap().push(first),
                )
                .await
            })
        };

        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        cancel.cancel();
        handle.await.unwrap();

        let flags = failure_flags.lock().unwrap().clone();
        assert_eq!(flags, vec![true, false, false]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_sink_write_that_did_not_land_is_resent_next_attempt() {
        // The extractor keeps returning the SAME url every attempt; the
        // sink fails (returns false, simulating a #298 generation mismatch
        // or a failed `set_metadata` call) on its first call and succeeds
        // on every call after. `run_url_refresh` must not treat the failed
        // write as sent -- it must retry the identical URL on the next
        // attempt rather than silently dropping it.
        let extractor = std::sync::Arc::new(ScriptedExtractor::new(vec![UrlExtraction::Url(
            "https://example.com/retry".to_string(),
        )]));
        let sink_calls = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink_attempts = std::sync::Arc::new(AtomicUsize::new(0));

        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let handle = {
            let extractor = extractor.clone();
            let sink_calls = sink_calls.clone();
            let sink_attempts = sink_attempts.clone();
            tokio::spawn(async move {
                run_url_refresh(
                    test_cfg(),
                    move |_attempt| {
                        let extractor = extractor.clone();
                        async move { extractor.next().await }
                    },
                    // Title changes every poll so the freshness skip never
                    // engages -- this test is about the sink's return
                    // value, not the skip path.
                    {
                        let mut n = 0u32;
                        move || {
                            n += 1;
                            Some(format!("title-{n}"))
                        }
                    },
                    move |url| {
                        let sink_calls = sink_calls.clone();
                        let sink_attempts = sink_attempts.clone();
                        async move {
                            sink_calls.lock().unwrap().push(url);
                            sink_attempts.fetch_add(1, Ordering::SeqCst) > 0
                        }
                    },
                    task_cancel,
                    |_outcome, _first| {},
                )
                .await
            })
        };

        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        cancel.cancel();
        handle.await.unwrap();

        let sent = sink_calls.lock().unwrap().clone();
        assert_eq!(
            sent,
            vec![
                Some("https://example.com/retry".to_string()),
                Some("https://example.com/retry".to_string()),
            ],
            "a sink write that returned false must be retried with the same URL"
        );
    }
}
