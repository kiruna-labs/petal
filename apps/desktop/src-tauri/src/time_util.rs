//! Wall-clock helpers (#143). Previously duplicated as private `now_ms()` /
//! `now_us()` fns in five modules (remote_control, session, diagnostics, rooms,
//! subscriber); consolidated here so there's one definition. Both saturate at
//! `u64::MAX` rather than truncating, and both return 0 if the clock is somehow
//! before the Unix epoch (unreachable in practice) instead of panicking.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Microseconds since the Unix epoch.
pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}
