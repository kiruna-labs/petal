# Petal patch to `tauri-nspanel`

Vendored from `ahkohd/tauri-nspanel` at the pinned rev
`a3122e894383aa068ec5365a42994e3ac94ba1b6` (the same rev Petal was pinned to
via a plain git dependency before this patch, `PanelLevel`/`v2.1.0`), with the
one fix described below. Pinned via
`[patch."https://github.com/ahkohd/tauri-nspanel"]` in
`apps/desktop/src-tauri/Cargo.toml` — **not** `[patch.crates-io]` like
`vendor/screencapturekit`/`vendor/livekit`/`vendor/libwebrtc`/
`vendor/webrtc-sys`, because `tauri-nspanel` is a git dependency, not a
crates.io one; `[patch.crates-io]` only intercepts the crates.io source.

The vendored copy was produced with `cargo package -p tauri-nspanel
--no-verify --allow-dirty` run against a fresh clone of the pinned rev, then
extracting the resulting `.crate` tarball — the same normalized-manifest
shape (`Cargo.toml` + `Cargo.toml.orig`, no `[workspace]`, no `examples/`)
already used by the other vendored deps in this directory, even though this
one was never actually published to crates.io.

## Activation-policy leak on a panel-build error (issue #705)

### The bug

In `src/builder.rs`, `PanelBuilder::build()` implements the `no_activate`
option (set by every panel this app builds — share borders, the hover pill,
remote-window chrome, `compositor.rs`) by flipping `NSApp`'s activation
policy to `.Prohibited` for the duration of the build, saving the previous
policy, then restoring it once the panel is fully configured:

```rust
let original_policy = if self.panel_config.no_activate.unwrap_or(false) {
    MainThreadMarker::new().map(|mtm| unsafe {
        let app = NSApplication::sharedApplication(mtm);
        let current_policy = app.activationPolicy();
        let _success = app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
        current_policy
    })
} else {
    None
};

// ...

let window = window_builder.build()?;   // <-- early return skips restore

// ... panel configuration ...

// Restore original activation policy if we changed it
if let Some(policy) = original_policy {
    // ...
}
```

`window_builder.build()?` early-returns on any window-build error, which
skips the restore block sitting at the bottom of the function entirely. Any
panel-build failure therefore leaks the whole app in `.Prohibited`
permanently for the rest of the process — no Dock icon, unactivatable, for
the entire remaining session. This isn't limited to one panel type; every
panel builder in this codebase goes through this same `no_activate` path.

### The fix

Added an RAII guard, `RestoreOnDrop<F>`, defined at the top of
`src/builder.rs`. It wraps a `restore` closure and runs it exactly once, on
`Drop` — covering every exit path (`Ok`, `Err` via `?`, or a future panic
while unwinding) instead of only the one exit the original code remembered
to write an explicit restore call for.

`build()` now constructs the guard immediately after flipping the policy —
before the fallible `window_builder.build()?` — capturing the original
policy by value in the closure:

```rust
let _activation_policy_guard = if self.panel_config.no_activate.unwrap_or(false) {
    MainThreadMarker::new().map(|mtm| {
        let app = NSApplication::sharedApplication(mtm);
        let original_policy = unsafe { app.activationPolicy() };
        let _success =
            unsafe { app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited) };
        RestoreOnDrop::new(move || {
            if let Some(mtm) = MainThreadMarker::new() {
                let app = NSApplication::sharedApplication(mtm);
                let _success = unsafe { app.setActivationPolicy(original_policy) };
            }
        })
    })
} else {
    None
};
```

The old tail-of-function restore block (the one the `?` used to skip) was
deleted — the guard's `Drop` now does that job unconditionally, on scope
exit, regardless of which return path is taken.

Nothing else changed. This is safe because:
- The guard only runs the exact same two AppKit calls
  (`NSApplication::sharedApplication` + `setActivationPolicy`) the original
  restore block ran, on the same `MainThreadMarker`-gated path — it doesn't
  change *what* gets restored, only *when* it's guaranteed to run.
- Every other branch of `build()` (window position/size/title, all the
  `panel.set_*` configuration calls) is byte-for-byte identical to upstream.
- Confirmed by diffing this vendored copy against a fresh `git clone` of
  `ahkohd/tauri-nspanel` at `a3122e894383aa068ec5365a42994e3ac94ba1b6`,
  packaged the same way: `src/builder.rs` is the only file that differs.

### Testing

`src/builder.rs` gained a `#[cfg(test)] mod petal_activation_policy_restore_tests`
exercising `RestoreOnDrop` directly against a helper (`flip_then_maybe_fail`)
that mirrors `build()`'s exact control-flow shape — flip a value, construct
the guard immediately after, then a step that can return early via `?`
before anything else runs:

- `restore_runs_when_the_fallible_step_succeeds`
- `restore_runs_when_the_fallible_step_returns_err` — the regression test
  for #705 itself: proves the restore fires even when the fallible step
  errors and exits early, which is exactly the property the old code lacked.
- `restore_runs_exactly_once`

This crate ships no test infrastructure of its own and, like the rest of
this codebase's native tests (see `platform/appkit.rs`'s
`main_thread_marker_guard_bails_gracefully_off_the_main_thread`), can't
exercise the real `NSApplication`/`MainThreadMarker` call inside a `#[test]`
body — Rust's default test harness runs every test on a spawned worker
thread, never the process's actual main thread, so `MainThreadMarker::new()`
is always `None` there and the real AppKit calls never fire either way. The
tests above instead validate the RAII mechanism `build()` relies on — that
`restore` runs on every exit path, not only a successful one — using the
literal `RestoreOnDrop` type `build()` constructs, decoupled only from the
AppKit specifics that can't run off the main thread.

Mutation-checked: temporarily deleted the `let _guard = RestoreOnDrop::new(...)`
line from the test helper (reproducing the pre-fix shape, where restoring
only happens after a successful return) — `restore_runs_when_the_fallible_step_returns_err`
failed as expected (`restore_runs_exactly_once` and
`restore_runs_when_the_fallible_step_succeeds` still passed, since neither
exercises the early-return path). Restored the guard; all three pass again.

**Not verified from this sandbox:** a live, one-machine check that forcing a
real panel-build failure in a running Petal build leaves the app's Dock icon
and `Cmd+Tab` reachability intact afterward (issue #705's "Human, one
machine" test). The fix and its unit-level regression test are verified;
this live pass is still needed before fully closing out that class of
evidence.

# Petal patch 2 (issue #824): `no_activate` no longer flips the activation policy

Upstream implemented `no_activate(true)` by flipping `NSApp`'s activation
policy Regular→Prohibited around `window_builder.build()`, then restoring it
(the #705 guard above hardened that restore). Measured live on macOS 26.5:
**each such flip makes the Dock register a DUPLICATE application tile** for
an LS-launched app — Petal's three startup `no_activate` panels produced
three "Petal" Dock icons for one process (tile-count sampling stepped +1 at
each panel-created log line; with the flip disabled, exactly one tile,
cleanly removed on quit). Creating windows under Prohibited also violates
AppKit's own contract for that policy ("may not create windows"), and the
flip could race/clobber `PETAL_ACCESSORY_UI`'s Accessory policy (#823).

The mechanism is replaced: `no_activate(true)` now appends
`.visible(false).focused(false)` to the window builder (before `window_fn`,
so an explicit caller `.visible(true)` still wins). A hidden build cannot
key-and-order-front, which is the focus steal (#677 class) the option
exists to prevent — per-window, no process-global state. Every
`no_activate` caller hides/reveals its panel itself, so none relied on
build-time visibility. With the flip gone, `RestoreOnDrop`, its tests, and
the whole #705 leak class are DELETED rather than guarded; #705's pending
"live panel-build-failure leaves the Dock icon intact" verification is moot.
A source-shape test (`petal_no_activate_mechanism_tests`) pins both halves:
the hidden+unfocused build must exist, and `build()` must never call
`setActivationPolicy` again.

## Re-vendoring / updating

If `tauri-nspanel` ever adopts an equivalent fix upstream, drop this vendor
dir and the `[patch."https://github.com/ahkohd/tauri-nspanel"]` entry, and go
back to a plain git dependency in `src-tauri/Cargo.toml`. To re-vendor a
newer rev with the same patches: clone the new rev, run `cargo package -p
tauri-nspanel --no-verify --allow-dirty`, extract the resulting `.crate`,
diff `src/builder.rs` against this file's copy, and re-apply **the #824
mechanism replacement** (delete any activation-policy flip in `build()`;
implement `no_activate` as `.visible(false).focused(false)` before
`window_fn`; keep the source-shape test). Do NOT re-apply upstream's
Prohibited dance in any form — it mints duplicate Dock tiles per panel.
