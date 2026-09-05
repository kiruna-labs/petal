//! Dev-only token minter for `web-harness/` (a browser-based LiveKit test
//! client -- see `web-harness/README.md`). Prints a single LiveKit JWT to
//! stdout so it can be copy-pasted into the web harness page's "access
//! token" field.
//!
//! This is deliberately just a thin CLI wrapper around the exact same
//! `transport::mint_access_token`/`transport::livekit_url` used by the real
//! app (`session.rs::join_room`) and by `publish_probe.rs` -- no new token
//! logic, no HTTP auth server. A copy-paste-the-token-from-a-CLI-run
//! workflow is the right amount of engineering for a dev/test tool (see the
//! task that added this: building a full auth backend for a harness whose
//! only user is a developer/agent driving a browser tab would be
//! over-engineering).
//!
//! ## Room naming
//!
//! The real app derives its LiveKit room name from a local, persisted
//! `RoomRecord` (`rooms::livekit_room_name`, e.g. `"petal-room-<id>"`) --
//! see `rooms.rs`/`session.rs::join_room`. This CLI does NOT reimplement
//! that local-persistence flow (reading/writing `rooms.json` from a
//! non-Tauri example binary is unnecessary ceremony for a test tool). Instead
//! it takes `--room` as a raw string used directly as the LiveKit room name.
//! To join the SAME room a real native Petal client is using, pass that
//! client's *derived* LiveKit room name (visible in its logs as `"session:
//! joined room '<name>' (livekit room '<livekit_room_name>')"`), not the
//! human-readable room name shown in the app's UI.
//!
//! Usage:
//! ```text
//! cargo run --example mint_token -- --room petal-room-abc123 --identity web-tester --publish --subscribe
//! ```
//!
//! Reads `LIVEKIT_URL`/`LIVEKIT_API_KEY`/`LIVEKIT_API_SECRET` from
//! `apps/desktop/.env` via `dotenvy`, same as `publish_probe.rs`/
//! `subscribe_probe.rs` -- never logs their values. Also prints
//! `LIVEKIT_URL` itself to stdout (not a secret -- it's the public wss://
//! endpoint the web harness also needs to paste in alongside the token).

use clap::Parser;

/// Mint a LiveKit access token for `web-harness/`'s browser test client.
#[derive(Parser, Debug)]
#[command(about = "Mint a LiveKit access token for the web-harness test client")]
struct Args {
    /// LiveKit room name to join. Pass the real app's *derived* LiveKit room
    /// name (see this file's module doc comment) to join the same room a
    /// native Petal client is in, or any string to test standalone.
    #[arg(long, default_value = "petal-web-harness-room")]
    room: String,

    /// Participant identity (must be unique per participant in the room).
    #[arg(long, default_value = "web-tester")]
    identity: String,

    /// Grant permission to publish tracks (video/audio). On by default --
    /// the web harness's whole point is publishing a synthetic shared
    /// window.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    publish: bool,

    /// Grant permission to subscribe to other participants' tracks. On by
    /// default so the harness can also render the native app's shares back,
    /// proving the receive side symmetrically.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    subscribe: bool,
}

fn main() {
    // Load apps/desktop/.env without ever printing its contents (same
    // pattern as publish_probe.rs/subscribe_probe.rs).
    let _ = dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));

    let args = Args::parse();

    let url = desktop_lib::transport::token::livekit_url().unwrap_or_else(|e| {
        eprintln!("Failed to read LIVEKIT_URL: {e}");
        eprintln!("Check apps/desktop/.env has LIVEKIT_URL set.");
        std::process::exit(1);
    });

    let token = desktop_lib::transport::mint_access_token(
        &args.identity,
        &args.room,
        args.publish,
        args.subscribe,
    )
    .unwrap_or_else(|e| {
        eprintln!("Failed to mint access token: {e}");
        eprintln!("Check apps/desktop/.env has LIVEKIT_API_KEY / LIVEKIT_API_SECRET set.");
        std::process::exit(1);
    });

    println!("room:     {}", args.room);
    println!("identity: {}", args.identity);
    println!("url:      {url}");
    println!("token:    {token}");
}
