//! The LiveKit access token for the CURRENT room join, kept so AI chat can
//! prove to `/api/ai-token` who is asking (#655).
//!
//! ## Why this exists
//!
//! `/api/ai-token` will not mint a Gemini token for an unauthenticated caller.
//! It requires the caller's LiveKit JWT as a bearer, verifies the signature
//! against the backend's own API secret, and checks that the token's room and
//! identity match the request — so a caller cannot ask for a token in someone
//! else's name. The desktop client used its JWT to connect and then dropped it,
//! so there was nothing to present; this holds onto it.
//!
//! ## Lifecycle, and why it is narrow
//!
//! A cached credential that outlives its room is a hazard: it would let a later
//! request be made against a room this process has left. So the value is
//! written on a successful join and cleared on leave, and [`current`] is the
//! only reader. It lives in memory only — never logged, never persisted, never
//! sent anywhere but the backend's own `/api/ai-token`.

use std::sync::{Mutex, OnceLock};

fn cell() -> &'static Mutex<Option<String>> {
    static TOKEN: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    TOKEN.get_or_init(|| Mutex::new(None))
}

/// Record the access token minted for the room this process just joined.
/// Replaces any previous value — a new join supersedes the old room outright.
pub fn remember(token: String) {
    if let Ok(mut guard) = cell().lock() {
        *guard = Some(token);
    }
}

/// Drop the cached token. Called on leave, so a token can never be presented
/// for a room this process is no longer in.
pub fn forget() {
    if let Ok(mut guard) = cell().lock() {
        *guard = None;
    }
}

/// The current room's access token, if joined.
pub fn current() -> Option<String> {
    cell().lock().ok().and_then(|g| g.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    // These share one process-global cell, so they run as a single test to
    // stay deterministic under the parallel test harness.
    #[test]
    fn remember_replace_and_forget() {
        forget();
        assert!(current().is_none(), "starts empty");

        remember("jwt-room-one".into());
        assert_eq!(current().as_deref(), Some("jwt-room-one"));

        // A second join must supersede the first, not accumulate.
        remember("jwt-room-two".into());
        assert_eq!(current().as_deref(), Some("jwt-room-two"));

        // Leaving must leave nothing behind to present later.
        forget();
        assert!(current().is_none(), "leave must clear the credential");
    }
}
