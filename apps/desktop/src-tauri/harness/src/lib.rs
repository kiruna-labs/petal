//! Petal M3 self-evaluation harness (SPEC.md §7).
//!
//! ## Why a separate crate, not a module inside `desktop_lib`
//!
//! This harness is a standalone testing TOOL, not part of the shipping app:
//! it links `clap`/spawns many concurrent room connections/emits JSON
//! scorecards, none of which belong in the Tauri app binary. It lives as a
//! sibling crate (`apps/desktop/src-tauri/harness/`) with its OWN
//! `[workspace]` table in `Cargo.toml` (rather than folding into
//! `../Cargo.toml`, which has no `[workspace]` of its own -- `desktop_lib` is
//! Tauri's single-package convention, not a workspace root) so `cargo
//! build`/`cargo test` from `../` (the real app) are completely unaffected
//! by anything added here, and this crate's own `cargo build`/`cargo test`
//! run independently. See that Cargo.toml's comment for more.
//!
//! ## Reuse vs. new code
//!
//! - **Live-I/O opt-in:** [`bot`] and the `petal-harness` runner compile with
//!   `--features live-io` and reuse `desktop_lib::transport` directly. The
//!   default crate build is still the CI-safe scorecard/metrics slice and
//!   performs no LiveKit or window I/O.
//! - **New in this crate:**
//!   - [`pattern`] -- synthetic BGRA test-pattern generator (pure, unit-tested).
//!   - [`metrics`] -- latency/freeze/jank computation from a frame-sample
//!     sequence (pure, unit-tested).
//!   - [`impairment`] -- network impairment profile config + `tc netem`
//!     argument translation + event-profile timelines (pure, unit-tested;
//!     does NOT shell out to `tc` -- see that module's doc comment for why).
//!   - [`scorecard`] -- per-scenario result shape, JSON (de)serialization,
//!     and the baseline regression gate (pure, unit-tested).
//!   - `petal-harness` live runner -- opt-in `live-io` binary that publishes
//!     synthetic shares and writes a fresh scorecard from received frames.
//!
//! ## What this task's environment could NOT verify live
//!
//! This session's environment has a confirmed bug where any process that
//! opens a real network connection or window hangs unkillably -- so nothing
//! in [`bot`] (or the `petal-harness` binary built on top of it, in
//! `src/bin/harness.rs`) was run against a real LiveKit room in this task.
//! Everything in [`pattern`], [`metrics`], [`impairment`], and [`scorecard`]
//! IS covered by real `cargo test` runs (no live I/O in any of those
//! modules' test suites) -- see each module's own tests for what's proven.
//! Note: later in this same session, `cargo test`'s own test *binary*
//! (an unbundled raw executable, same category as other native probes this
//! project's CLAUDE.md documents hanging) became unreliable to actually
//! execute in this environment, independent of whether its tests do I/O --
//! `cargo check`/`cargo build` for this crate remain the reliable
//! correctness signal here; re-run `cargo test --lib` on a healthy machine
//! (fresh disk space, no accumulated zombie processes) to see it pass.

pub mod impairment;
pub mod metrics;
pub mod pattern;
pub mod scorecard;

#[cfg(all(feature = "live-io", target_os = "macos"))]
pub mod bot;
