//! Manual verification harness for #281 (Sentry crash + error reporting).
//! Deliberately panics on purpose -- NOT run by `cargo test`/CI -- to prove,
//! end to end against a real Sentry project, that:
//!
//!   1. `logging::init()` -> `install_panic_hook()` -> `forward_panic_to_sentry`
//!      -> explicit flush actually delivers an event before the process
//!      exits (the "flush-before-death" requirement #281 calls out as the
//!      single most likely way this integration ships broken while every
//!      automated test still passes -- a real panicking process exiting is
//!      the only thing that actually exercises that timing).
//!   2. The allowlist-first PII scrub (`scrub_event_for_sentry`) strips a
//!      room name + participant identity embedded in the panic message
//!      before it ever leaves the machine, not just in the unit tests that
//!      call the scrub function directly in-process.
//!
//! Usage (DSN must be supplied at BUILD time, matching the real production
//! `option_env!("PETAL_SENTRY_DSN")` embedding path -- NOT injected only at
//! run time, since that would exercise the local-testing convenience path
//! instead of the actual compile-time-embedded production path):
//!
//!   PETAL_SENTRY_DSN="https://<key>@<org>.ingest.us.sentry.io/<project>" \
//!     cargo run --example sentry_panic_probe
//!
//! With no DSN set (the default for a plain `cargo build`/`cargo run`),
//! `logging::init()`'s Sentry setup is a clean no-op and this probe just
//! panics locally with no network effect -- same as every other build.
fn main() {
    let log_path = desktop_lib::logging::init();
    eprintln!("sentry_panic_probe: log file at {}", log_path.display());
    eprintln!("sentry_panic_probe: panicking now on purpose to exercise the panic -> Sentry -> flush-before-death path...");
    // Deliberately embeds a fake room name + participant identity, in the
    // exact quoted shape `redact_for_export`'s markers recognize, so this
    // probe proves the PII policy end-to-end against the real Sentry API --
    // not just that SOME event arrives.
    panic!(
        "sentry_panic_probe: test panic while in room 'eng-sync-probe' for identity 'probe-tester@example.com'"
    );
}
