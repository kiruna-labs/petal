//! `PETAL_TEST_SHARE_PAUSE=<afterMs>:<durationMs>` (#391): once per process,
//! drop every window-share frame for `durationMs`, starting `afterMs` after
//! the first window frame was pushed. It manufactures a sender-side freeze so
//! the share-freeze soak (nightly-loopback.yml, input `share_freeze_pause`)
//! can be shown to FAIL. Debug builds only; never a product setting.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const TRACK_PREFIX: &str = "petal-window-";

pub(crate) fn parse(raw: Option<&str>) -> Option<(Duration, Duration)> {
    let (after, duration) = raw?.trim().split_once(':')?;
    let after = after.trim().parse::<u64>().ok()?;
    let duration = duration.trim().parse::<u64>().ok().filter(|ms| *ms > 0)?;
    Some((Duration::from_millis(after), Duration::from_millis(duration)))
}

/// Pure window test: is `elapsed` (since the first pushed frame) inside the pause?
pub(crate) fn in_pause(elapsed: Duration, after: Duration, duration: Duration) -> bool {
    elapsed >= after && elapsed < after + duration
}

fn config() -> Option<(Duration, Duration)> {
    static CONFIG: OnceLock<Option<(Duration, Duration)>> = OnceLock::new();
    *CONFIG.get_or_init(|| parse(std::env::var("PETAL_TEST_SHARE_PAUSE").ok().as_deref()))
}

/// True when this window-share frame must be dropped. Logs the start and the
/// end of the pause once each, so the soak's evidence names the exact window.
pub(crate) fn should_drop(track_name: &str) -> bool {
    if !track_name.starts_with(TRACK_PREFIX) {
        return false;
    }
    let Some((after, duration)) = config() else {
        return false;
    };
    static FIRST_PUSH: OnceLock<Instant> = OnceLock::new();
    // 0 = before, 1 = pausing, 2 = done.
    static PHASE: AtomicU8 = AtomicU8::new(0);
    let elapsed = FIRST_PUSH.get_or_init(Instant::now).elapsed();
    if in_pause(elapsed, after, duration) {
        if PHASE.swap(1, Ordering::Relaxed) == 0 {
            log::warn!(
                "TEST share pause: dropping every window-share frame for {}ms (PETAL_TEST_SHARE_PAUSE, red proof only)",
                duration.as_millis()
            );
        }
        return true;
    }
    if elapsed >= after + duration && PHASE.swap(2, Ordering::Relaxed) == 1 {
        log::warn!("TEST share pause: ended; window-share frames flow again");
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_after_and_duration() {
        assert_eq!(
            parse(Some("60000:2000")),
            Some((Duration::from_millis(60_000), Duration::from_millis(2_000)))
        );
        assert_eq!(
            parse(Some(" 0 : 1500 ")),
            Some((Duration::ZERO, Duration::from_millis(1500)))
        );
    }

    #[test]
    fn rejects_empty_malformed_and_zero_duration() {
        for raw in [None, Some(""), Some("2000"), Some("a:b"), Some("1000:0"), Some("-1:2000")] {
            assert_eq!(parse(raw), None, "{raw:?}");
        }
    }

    #[test]
    fn pause_window_is_half_open() {
        let (after, duration) = (Duration::from_millis(100), Duration::from_millis(50));
        assert!(!in_pause(Duration::from_millis(99), after, duration));
        assert!(in_pause(Duration::from_millis(100), after, duration));
        assert!(in_pause(Duration::from_millis(149), after, duration));
        assert!(!in_pause(Duration::from_millis(150), after, duration));
    }
}
