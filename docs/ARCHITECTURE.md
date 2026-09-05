# Petal architecture

How the app is wired, for someone new to the code. For *why* a given fix exists
(the crash histories, verification transcripts, and real-vs-stand-in
breakdowns) see `internal/archive/CLAUDE-full-log-through-2026-07-02.md`.
For build/run/verify rules see the root `CLAUDE.md`.

## Shape

Tauri 2 app with supported macOS and Windows desktop clients; the Windows
native surface is complete for the current product feature set. Rust core
(`apps/desktop/src-tauri`, crate `desktop`) plus
a Svelte 5 / TypeScript / Vite SPA frontend (`apps/desktop/src`, SvelteKit with
`adapter-static`, `ssr = false`). A managed SFU (LiveKit) carries media and
data; a small Vercel backend (`backend/`) mints scoped room tokens so the app
never holds the LiveKit API secret. A browser LiveKit participant
(`web-harness/`) stands in as a second peer for headless testing.

**Shared UI + logic:** design tokens (`shared/ui/tokens.css`), presentational
components (`shared/ui/components/`), and pure logic (`shared/logic/` — meeting
codes, join input, local echo) are the SINGLE SOURCE imported by BOTH the
desktop app and `web-harness` via the `@petal/shared` alias. Per-client app
shells — SvelteKit routes + Tauri IPC vs the web client's hand-rolled meeting
UI — stay separate; everything shared comes from `shared/`.

The defining behavior: a shared window renders on every *other* participant's
machine as a real, independently movable borderless native window — not a grid
tile. That receiver-side compositor (`compositor.rs` + `native_display.rs`) is
what the whole design is organized around.

## Rust module map

Roughly `platform → capture/transport → session → chrome → diagnostics`.

- **`platform/`** — the leaf FFI layer, no app logic.
  - `cg.rs` — CoreGraphics: `WindowFrame`, cursor/button/Escape state,
    `frame_for_window_id()`, and the on-screen window-stack snapshot (the
    single `CGWindowList` FFI decl; #142).
  - `appkit.rs` — `NSWindow`/`NSView`/layer micro-FFI with SAFETY notes (#139).
  - `mod.rs::on_main(app, tag, f)` — the canonical "marshal onto the AppKit
    main thread + log on failure" helper and the home of the main-thread rule
    (#143).
  - `power.rs` — `DisplaySleepAssertion`: IOKit `IOPMAssertionCreateWithName`/
    `Release` RAII wrapper (`kIOPMAssertionTypePreventUserIdleDisplaySleep`),
    held for the duration of `session::room::join_room`..`leave_room`
    (#259/#264).
- **`capture.rs`** — one `SCStream` per shared window (ScreenCaptureKit), BGRA
  frames to a callback.
- **`transport/`** — everything LiveKit.
  - `token.rs` — asks the backend for a scoped room token + signaling URL.
  - `backend_http.rs` — shared reused `reqwest::Client` + `{"error":…}` decoder
    (#143), used by `token.rs` and `rooms.rs`.
  - `publisher.rs` — `RoomConnection`, `PublishedTrack`, `ShareQuality`; window
    capture → H.264 publish, focus-weighted quality (unpublish+republish).
  - `subscriber.rs` — `start_compositor_feed`: remote video track → decoded
    `Native` CVPixelBuffer → compositor, no CPU copy.
  - `audio.rs` — mic capture (`PlatformAudio`/WebRTC ADM), Opus publish, real
    mute, device hot-swap.
  - `camera.rs` — native webcam capture (AVFoundation, macOS-gated).
- **`native_display.rs`** — the zero-copy decode-to-display path: CVPixelBuffer
  → `CMSampleBuffer` → `AVSampleBufferDisplayLayer`.
- **`compositor.rs`** — one borderless `NSPanel` per subscribed remote shared
  window; cascade placement, aspect-locked resize, hide+retire lifecycle (never
  destroy a panel — see crash class 2 in root `CLAUDE.md`). `push_frame`'s
  display-layer enqueue is gated by a global `DisplayEnqueueGate`
  (`set_display_enqueue_paused`/`display_enqueue_paused`), paused while the
  receiver's own display is asleep (#259/#264 — see crash class 5 in root
  `CLAUDE.md`).
- **`session/`** — the orchestration core (`SessionState`, Tauri-managed).
  - `mod.rs` — state, mic-mute intent, generation counters.
  - `share.rs` — `ActiveShare`, start/stop share, share cap (4), focus model,
    resize republish.
  - `room.rs` — `join_room`/`leave_room` + the per-connection watcher fan-out
    (telepointer, presence, resilience, audio, compositor feed).
  - `camera.rs` — camera publish start/stop.
  - `commands.rs` — the `#[tauri::command]` wrappers.
- **Native chrome / UI surfaces** — `hover_tab.rs` (the fixed 40×40 right-edge
  Share/Stop rail, threshold-based drag/preset placement, native-options hold, and cursor tracker), `share_border.rs` (macOS identity border on a shared
  window), `menubar.rs` (the macOS `NSStatusItem` pill + popover),
  `region_window.rs` (the cross-platform Petal View selector and its
  label-addressed title-bar controls), `windows_share_overlay.rs` (the
  Windows sharer telepointer/custom-indicator surface and the shared
  WinEvent/native follower for both the local border and hover tab),
  `control_consent.rs`/`control_consent_windows.rs` (the dedicated queued
  non-activating remote-control consent prompt), `window_picker.rs`,
  `network_cockpit.rs`, `main_window.rs`, `dev_telepointer.rs`.
- **`test_cockpit/`** — the env-gated scenario/journey test harness; see
  `docs/TESTING.md` and `internal/docs/COCKPIT_RUNBOOK.md` for detail.
- **Collaboration/state** — `telepointer.rs` (data-channel cursors);
  `remote_control_core.rs` (portable wire contract, grant identity,
  authorization/reliability/sequencing/held-input state, and the
  `PlatformControl`/`ControlSurface` seams); `remote_control.rs` (the shared
  Tauri/LiveKit adapter plus macOS Accessibility/SkyLight replay);
  `windows_remote_control.rs` (Windows HWND/PID/integrity/secure-field checks,
  capability routes, and serialized `SendInput` replay); `presence.rs`;
  `resilience.rs` (network-change → proactive
  reconnect + toasts; system sleep/wake → active-share refresh; screen
  sleep/wake → `compositor::set_display_enqueue_paused`, #259/#264); `rooms.rs`
  (durable `rooms.json` + occupancy). Remote-control packets are untrusted wire
  input: keep platform injection out of the portable module and preserve the
  fail-closed target/share/grant checks.
- **Diagnostics/infra** — `diagnostics.rs`, `window_diag.rs` (window-stack
  logging), `logging.rs` (`fern` → `~/Library/Logs/Petal/petal.log`; also owns
  off-device Sentry crash/error reporting, #281 — compile-time-embedded DSN,
  panic + ObjC-exception hook forwarding, allowlist-first PII scrub),
  `permissions.rs`, `shortcuts.rs` (global toggle shortcut), `deep_link.rs`
  (`petal://join/<credential>`), `autotest.rs` (env-gated debug driver),
  `webview_transparency.rs`, `window_resize.rs`, `gallery_bridge.rs`, `quit.rs`.
- **Utils** — `sync_ext.rs` (`lock_unpoisoned()`), `time_util.rs`
  (`now_ms`/`now_us`).

### Windows modules (all `#[cfg(target_os = "windows")]`)

The Windows client provides the native capture, sharing, compositor, control,
and overlay surfaces described below.

- `windows_capture_target.rs` — WGC item resolution (window vs display,
  `CreateForWindow` vs `CreateForMonitor`), with disposable tokens for stale-
  input safety.
- `windows_screen_capture.rs` — unified WGC window/display capture; one D3D11
  capture thread per session (COM MTA), cached borderless-consent policy, and
  fail-safe `System`/`Petal` indicator selection.
- `windows_compositor.rs` — receiver compositor: each remote share is a Tauri
  `WebviewWindow` on the shared `compositor/surface.html` route (same header UI
  as macOS) with the decoded video in a native child HWND below the header;
  dedicated Win32 message-loop thread; CPU I420→BGRA→D3D11 present;
  `DXGI_ERROR_DEVICE_REMOVED` recovery. Decoded frames are admitted through a
  per-window latest-frame mailbox (replacement-in-place, one frame per open
  window, round-robin drain) separate from the ordered lifecycle/geometry
  command lane, so a frame burst can never fill the command queue or block a
  Tauri move/resize callback (those callbacks are nonblocking + coalescing with
  a per-tick dirty-geometry reconcile). Its telepointer snapshot carries the
  surface and sibling-overlay HWNDs so the sender can select one topmost shared
  surface under an overlapping cursor. The sibling control webview also owns
  remote Draw input when `__petalDrawSetActive` is enabled; received strokes
  render through the click-through pointer webview.
- `region_window.rs` — cross-platform transparent Petal View selector;
  Windows registers its HWND before showing it, macOS registers its CGWindowID,
  and title-bar Options/Share/Stop commands address the selector by Tauri
  label. Its registered rectangle blocks ordinary hover tabs through the
  hollow interior. Follow-cursor placement keeps the surface hit-testable
  through the consumed click and mouse-up, then restores dynamic hollow
  click-through.
- `control_consent_windows.rs` — hidden singleton consent WebViewWindow,
  positioned at the cursor monitor's work-area top center and shown with
  `SWP_NOACTIVATE`; the route owns typed ordinary/escalation queueing and
  fail-closed timeout handling.
- `windows_share_overlay.rs` — the single Windows WinEvent/message-pump
  follower for the local sharer border, sharer telepointer, and idle hover tab.
  It reads one DWM-visible physical source frame per reconcile; the border uses
  that full frame while the tab applies the persisted/previewed normalized rail
  offset and projects into the current monitor `rcWork`, hiding fail-closed if
  no safe scaled 40×40 rectangle or native placement exists. Ordinary sources
  use a non-topmost tab inserted immediately above the source, so unrelated
  windows above a background source naturally occlude it. Elevated or
  integrity-unknown sources use a temporary `HWND_TOPMOST` fallback and
  hit-test through the tab; while the elevated source remains foreground, its
  cached tab stays attached while the source is focused or actively shared,
  even if the underlying hit is another window. Otherwise the normal hit-test
  path retargets or hides it. Its
  250ms timer is only a missed-event/display fallback. The shared WinEvent
  admission uses the event object/child fields, active source/follower context,
  and a `GetAncestor(hwnd, GA_ROOT) == hwnd` top-level check for
  `EVENT_OBJECT_REORDER`; GA_ROOT equality alone is not a general proof that
  browser/render/details-pane churn is harmless. The hover tab's own reorder is
  ignored. Accepted foreground and top-level reorder events queue the full
  tracker reconcile; source and unrelated top-level events remain eligible. Verified
  same/lower-integrity ordinary window shares use Win32 ownership; elevated or
  integrity-unknown sources get only a passive unowned telepointer while WGC
  keeps its system border. Display shares retain their existing topmost policy.
  The route
  renders telepointers and Draw strokes through
  `compositor/pointer.html?surface=sharer`; readiness (shown, capture-excluded,
  custom) gates WGC border suppression, and the surface remains click-through
  except while the sharer's Draw mode is active and is removed with the share.
- `windows_remote_control.rs` — generic window pointer/wheel/keyboard/text
  replay; class-specific UIA Invoke/Scroll remains an optional capability, not
  an app admission whitelist. Windows controller state separates an active
  control session from individual operation feedback: replay refusal never
  removes a grant or disables the control overlay, while explicit
  stopped/disabled/share/room lifecycle paths still terminate and clean up.
  Window-share wheel is a cursor-preserving `WM_MOUSEWHEEL`/`WM_MOUSEHWHEEL`
  route delivered via `SendMessageTimeoutW` (synchronous, `SMTO_ABORTIFHUNG` +
  250ms — the same mechanism Chromium itself uses to redirect wheel between
  its own windows, `ui/base/win/mouse_wheel_util.cc`): the destination is the
  shared window's own SCROLLABLE descendant under the cursor
  (`EnumChildWindows` + `GetScrollInfo`/`WS_*SCROLL`), with the top-level
  window as fallback — no focus, no `SetCursorPos`, no `SendInput`, no
  fallback. Resolving the scrollable child (not just any child) is what makes
  the wheel land on the actual editor/render widget for both Chromium apps
  (browser and Win11 Notepad), and `SendMessageTimeoutW` (rather than
  `PostMessageW`) is required because a bare posted wheel to Chrome's legacy
  `Chrome_RenderWidgetHostHWND` is ignored by Chromium's input routing, while
  a direct window-proc invocation is not. The aim is validated only against
  the target's OWN client area (`ScreenToClient`/`GetClientRect` —
  z-order-independent), so a covering window neither blocks nor redirects the
  message: delivery is addressed to the target's own window by ID, and
  occlusion is deliberately never checked for ID-addressed injection. Known
  Chromium limitation: at a point physically covered by another window on the
  sharer's desktop, the browser's render widget still ignores the wheel
  because Chromium reroutes wheel input to the window under the pointer (the
  covering window owns that point); uncovered points scroll normally, and
  non-Chromium apps (e.g. Win11 Notepad) scroll even when covered. A
  successful delivery reports `submitted` (never `applied`). Occlusion/covered
  checks apply only to global-cursor/foreground routes (pointer/keyboard via
  `SendInput`), which genuinely need them. Display-share wheel retains the
  serialized global
  `SetCursorPos` + marked `SendInput`
  route, scoped to a point inside the shared display. Other routes never
  silently fall through to
  another injection route.
- `windows_audio_device.rs` — WASAPI mic capture + playout (behind
  `transport::audio`).
- `transport/windows_camera.rs` — Media Foundation camera capture.
- `session_stub.rs` — the real Windows session: WASAPI mic publication, remote
  playout, MF camera, WGC share publication on the shared LiveKit connection
  (NOT a stub).
- `autofill.rs` — disables WebView2 autofill engine-wide.

Windows control modes are host-authoritative: cursor-preserving window-local
control is the default, and explicit sharer-approved full-pointer control is
available when continuous pointer input is needed. Escalation is always a new
controller request plus sharer approval, never an operation fallback. The
cursor-preserving wheel route uses `SendMessageTimeoutW` window delivery; every
route remains fail-closed when target validation or observed application effect
is unavailable.

Shared: `window_source.rs` (window enumeration: CGWindowList on macOS, native
Windows enumeration), `share_target.rs` (the central picker/hover eligibility
classifier), `permissions.rs` (real TCC on macOS, granted-stubs on
Windows), `updater.rs` (Mach-O guard on macOS, NSIS PE guard on Windows),
`logging.rs` (`~/Library/Logs/Petal` vs `%APPDATA%\Petal\logs`; `open -R` vs
`explorer.exe /select`).

## Native window / panel inventory

| Label | Platform/kind | Purpose |
|---|---|---|
| `main` | macOS/Windows `WebviewWindow` | the SPA: onboarding, `/main`, `/meeting/[room]`, `/settings` |
| `hover-tab` | macOS `NSPanel`; Windows non-topmost `WebviewWindow` | fixed 40×40 right-edge Share/Stop rail; Windows inserts it immediately above the source so unrelated foreground windows occlude it naturally; pointer drag and native Top/Center/Bottom presets change one global vertical offset; right-click and keyboard shortcuts open the native options menu |
| `share-border` | macOS `NSPanel` | macOS identity border drawn around a window you're sharing |
| `share-bar-*` | macOS `NSPanel` × active shares | full-width native bar above a shared local window |
| `menubar-popover` | macOS `NSPanel` | the menubar pill's popover (roster + controls) |
| `region-window-*` | macOS/Windows transparent `WebviewWindow` | Petal View selector, title-bar Share/Stop, and region placement surface |
| `petal-sharer-pointer-*` | Windows transparent `WebviewWindow` | local telepointers/Draw plus the optional Petal identity capture border; verified same/lower-integrity ordinary windows use Win32 ownership, elevated or integrity-unknown sources keep WGC's system indicator and receive only a passive unowned telepointer, and lost replacement readiness fails back to WGC |
| `petal-remote-*` surface | macOS `NSPanel`; Windows `WebviewWindow` + child HWND | remote shared-window video surface keyed by `(owner_identity, window_id)` |
| `petal-control-*` | Windows transparent `WebviewWindow` sibling | authenticated remote-control input surface for a remote share |
| `petal-pointer-*` | macOS/Windows transparent `WebviewWindow` sibling | remote telepointer/Draw overlay keyed by `(owner_identity, window_id)` |
| `network-cockpit` | macOS/Windows `WebviewWindow` | diagnostics cockpit |
| `window-picker` | macOS/Windows `WebviewWindow` | window picker surface |
| `ai-chat-panel` | macOS `NSPanel`; Windows `WebviewWindow` | AI-chat panel singleton |
| `dev-telepointer` | macOS/Windows `WebviewWindow` when enabled | dev-only telepointer harness |

On Windows, a `region-window-*` selector starts with `WDA_NONE`, so its frame,
title bar, and controls are visible to supported screen recorders while idle.
An active display-region share holds a scoped `WDA_EXCLUDEFROMCAPTURE` lease on
that selector until WGC capture teardown completes; if the lease cannot be
acquired, the share retains the WGC system indicator fallback. This affinity is
per selector and does not affect ordinary window/display shares.

Plus the macOS menubar pill itself (`NSStatusItem`, custom-drawn, not a
panel). On Windows the compositor has no `NSPanel`/`tauri_nspanel`: remote
shares are `WebviewWindow`s with a native child HWND for video, sibling control
and pointer webviews, and the local sharer overlay. See
`docs/WINDOWS_NATIVE_SURFACE_AUDIT.md` for the per-surface policy
matrix and ranked follow-ups.

### Hover-tab rail policy

The hover tab is a single native 40×40 surface. Its horizontal attachment remains
at the source window's right edge: it uses the outside slot when the platform
work area permits it and otherwise insets into the right edge. A normalized
vertical offset (`0` top, `0.5` center, `1` bottom) is shared by Windows and
macOS, stored in `share-preferences.json`, and applied source-relatively before
platform work-area clamping. Pointer motion below 6px remains Share/Stop;
movement at or above the threshold enters a drag, freezes the follower,
previews through the native adapter, and commits only on pointer-up.
Escape, pointer cancellation, lost capture, source loss, and room leave restore
the prior offset. On Windows the hover tab is never globally topmost: each
native placement uses the source's normal/topmost z-order band and inserts the
tab immediately above that source, so a foreground window covering the tab
naturally occludes it. Accepted foreground/top-level reorder events queue the
coalesced tracker to reconcile this adjacency; the tab's own reorder event is
ignored. Anchor or placement failure hides the tab fail-closed.
Top/Center/Bottom are keyboard-accessible entries in the existing system-native
menu; Petal View's label-addressed Options menu does not include hover-tab
placement entries.

**Pushing data into a compositor child webview (header/pointer overlays):** use
`webview.eval("window.__petalX(<json>)")`, NOT the Tauri event bus — `emit`/
`emit_to` do not reach `tauri_nspanel` child webviews. Windows Draw follows the
same path: the existing authenticated `petal.draw` protocol is reused without
wire changes, with remote strokes delivered to the remote pointer overlay and
local-owner strokes delivered to the sharer overlay. (Full story in the
archived log.)

## Tauri command & event catalog

The authoritative, drift-proof registry is **`apps/desktop/src/lib/ipc.ts`**
(`COMMANDS` + `EVENTS`, #132) — every frontend `invoke`/`listen` goes through
it, and it is kept in lockstep with the Rust `#[tauri::command]` handlers in
`lib.rs::run()`'s `generate_handler!`. Read that file for the current list
rather than trusting a copy here. Registration is per-platform: macOS
registers the full macOS surface (`lib.rs:809`), Windows a parallel set
including `windows_compositor::*` (`lib.rs:1229`); `ipc.ts` documents the
union the frontend actually calls. High-level groups:

- **Permissions:** `check_/request_` × screen-recording/microphone/camera.
- **Sharing:** `list_shareable_windows`, `capture_window_thumbnail`,
  `toggle_window_share`, `hover_tab_drag`, `share_window`, `shared_window_ids`,
  `update_share_border_frame`, `region_share_state`, `toggle_region_share`,
  `region_placement_active`.
- **Rooms/session:** `list_rooms`, `create_room`, `rename_room`,
  `list_room_occupancy`, `join_room_command`, `leave_room_command`,
  `current_room`, `room_presence`.
- **Media:** `start/stop_camera_publish_command`, `toggle_menubar_mic`,
  `set_mic_muted`, `list_audio_devices`, `set_audio_devices`.
- **Compositor chrome:** `compositor_activate_window`/`_start_drag`/
  `_pop_out`/`_fit_to_source`. (`_toggle_collapse` removed in #675 -- the
  Collapse feature is gone; the yellow-dot hide button is the only
  "get it out of the way" affordance now.)
- **Remote control:** `remote_control_send`/`_set_active`/`_answer_consent`/
  `_answer_escalation`/`_revoke`/`_allowed`/`set_remote_control_allowed`, plus
  native-only `remote_clipboard_copy`/`remote_clipboard_paste`.
- **Windows/diag:** `open_*_window`, `open_main_route`, `get_network_snapshot`,
  `get_event_journal`, `set_cockpit_open`, `record_video_stream_state`,
  `export_logs`, `log_window_stack_command`, `animate_main_window_resize`.

- **Events (Rust → frontend):** `hover-tab-update`/`-hide`, `share-error`,
  `share-picker-changed`, `share-state-changed`, `region-share-state-changed`,
  `region-placement-settled`, `region-placement-released`,
  `telepointer-update`, `presence-update`, `resilience-event`, `room-left`,
  `mic-mute-changed`, `remote-control-status`, `control-consent-requested`
  (typed ordinary-control or full-control-escalation prompt),
  `share-control-mode-changed`, `network-stats`, `journal-appended`.

## Remote-control control modes (Windows)

The sharer selects a per-share **control mode** (host-side policy) in the
window picker or the hover tab's native options menu; the receiver header shows
it read-only. **Cursor-preserving** (default) = real global click with a cursor
save/restore (`restore_is_safe`, once per gesture) + a per-controller parallel
keyboard message route for window shares (global injection for display shares);

the wheel stays cursor-preserving unchanged. **Full control** = the shipped
global route with the cursor staying. Escalation is **user-initiated**: the
controller requests, the existing non-activating `control-consent` panel
queues the prompt, and the sharer approves or denies (`set_share_control_mode`).
A 30-second timeout fails closed; Petal never auto-escalates. See
`docs/remote-control-trust-model.md` and `plans/windows-remote-control-modes.md`.

## Native remote clipboard boundary

`apps/desktop/src-tauri/src/remote_clipboard.rs` is the single native clipboard
seam. It owns platform plain-text reads/writes, clipboard sequence numbers,
recognized file-transfer detection, 1 MiB UTF-8 validation, short contention
locking, pending Copy correlation, and bounded Paste deduplication. macOS's
existing AX text-shortcut actuator in `remote_control.rs` delegates its
production pasteboard access to this seam; its fake `PasteboardBackend` remains
for the existing exact-window AX tests. Windows uses the direct
`clipboard-win` dependency for Unicode text, sequence numbers, and shell file
format detection.

`remote_control.rs` owns the authenticated routing and target replay. Native
keyboard Copy/Paste are normalized at the controller overlay and have fixed
boundary semantics: Copy is B→A and Paste is A→B. Copy uses a reliable JSON
request plus a targeted `petal.remote-control.clipboard-text` byte-stream
response; Paste is one targeted byte stream. The stream body is plain UTF-8
only, never placed in a `RemoteControlMessage`, status, result, cockpit ledger,
telemetry event, or log. Host Paste writes its clipboard before attempting the
existing target-safe native replay and deliberately does not restore it.

This is not a paired or provenance-aware B-local clipboard system. A keyboard
Copy followed by keyboard Paste is not guaranteed as native/lossless behavior;
users needing B-local semantics should use the target application's own
reachable in-window context menu, toolbar, or dropdown for both operations.
Browser peers ignore this native-only request/topic, and generic `kind: "text"`
continues to serve IME/composed input and existing harness behavior.

## Cross-language contracts

Slug/room-name derivation, track names (`petal-window-*`/`petal-camera-*`),
and the telepointer / remote-control wire formats are shared across Rust, the
backend, and the web harness, pinned by `contracts/petal-contracts.json` and
tests on both sides. See **`docs/CONTRACTS.md`** before changing any of them.

## Frontend routes

`/` (onboarding gate) · `/main` (menu) · `/meeting/[room]` (in-meeting; its
logic lives in `lib/meeting/*.svelte.ts` rune controllers, #137) · `/settings` ·
plus native-surface routes (`/menubar-popover`, `/compositor/*`, `/window-picker`,
`/network-cockpit`, `/region-window`) and `/dev/*` harnesses.
