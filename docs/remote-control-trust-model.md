# Remote control — trust model

_The living trust model for remote control (it began as the status page for
GitHub issue #30, which is now closed; the residual gaps below are tracked
here, not in an issue). This documents what the remote-control feature does
and does not defend against, so the trade-offs are explicit rather than
implicit in the code._

Remote control lets meeting participants drive the keyboard and mouse of a
window another participant is sharing. Multiple participants may control the
same shared window concurrently. Input rides the same LiveKit data-channel as
telepointers (topic `petal.remote-control`) but is gated more strictly because
it has real side effects on the sharer's machine. The portable remote-control
core and capability envelope are cross-platform. macOS uses Accessibility/SkyLight
routes; Windows uses `windows_remote_control.rs` for Win32/UIA target validation
and serialized `SendInput` replay — except window-share wheel, which is a
cursor-preserving `WM_MOUSEWHEEL`/`WM_MOUSEHWHEEL` route to the shared window's
own scrollable descendant under the cursor (with the top-level window as
fallback), delivered via `SendMessageTimeoutW` so it works even when the
shared window is covered (no focus/cursor movement, no fallback).

## Sharer-chosen control modes (Windows window/display shares)

The sharer selects a control **mode** per shared window/display at share time
(window picker) and can change it live from the hover tab's native options menu.
The mode is
**host-side authority**: it gates which delivery routes the host uses and is
never decided or changed by a controller on its own. Controllers need no replay
changes — the same packets are accepted, and a capable controller simply sees
the mode's envelope on its header.

- **Cursor-preserving (default).** Discrete gestures (click, drag-drop) are
  injected with real global input (`SetCursorPos` + serialized `SendInput`),
  then the host's cursor is restored to its prior position, guarded by a
  `restore_is_safe` check (restore only if the cursor is still within 6 px of
  where Petal last posted it — if the sharer physically moved the mouse, we
  back off rather than yank it), applied **once per gesture** (never between a
  drag Down and its Up). This mirrors the macOS `CursorTakeover` model: it is
  *cursor-restoring*, not truly cursor-preserving — the cursor briefly jumps
  to the remote point and back, and the clicked window is focused (which is
  what lets subsequent typing land). The wheel keeps its message route
  unchanged. Continuous pointer tracking (hover-follow) is full-control
  semantics and refuses with `notInjectible` rather than silently falling
  back.

  **Keyboard (window shares): per-controller focus target.** Each controller
  has a private focus target — the window/component it is addressing (its
  share target / last accepted cursor-preserving click), which deliberately
  may differ from the desktop's actual focused window. When that target is the
  foreground window, real global input is used (reliable, non-intrusive);
  otherwise a best-effort `PostMessage` route injects keys/`WM_CHAR` into the
  target **without stealing the sharer's focus**, enabling **parallel keyboard
  input** (the controller types into its target while the sharer keeps working
  in another window). Because the target is explicit and revalidated before every send, a key can never land in a wrong (merely-focused) window, and an unconsumed best-effort key never interrupts other users. **Display shares do not use this**: keyboard/text is global injection into the verified foreground target, for simplicity.
- **Full control (stronger).** The shipped global route: the cursor stays at
  the controller's point (`SetCursorPos` + serialized `SendInput`), enabling
  continuous pointer tracking. Reaches apps/gestures message injection cannot.

This model covers explicit per-controller escalation; it does not define a
shared open-floor lease.

### Escalation is user-initiated, never Petal's decision

When a cursor-preserving mode cannot effectively inject an event (e.g. a
continuous pointer move, or an unconsumed parallel key), the controller
receives a `notInjectible`/`unsupportedRoute` result and can surface
"Request full control". The controller sends an escalation request and the
sharer approves or denies it in the existing non-activating `control-consent`
panel. Approval flips the per-share mode to full control
(`set_share_control_mode`); denial keeps cursor-preserving. The prompt is
identified by `(kind, window, controller)`, expires after 30 seconds, and
revalidates the live share and active grant before changing mode. A refusal or
an escalation request is a per-operation result, never a session-demoting
status, and **Petal never auto-escalates or silently falls back** to the
stronger mode.

## The model in one line

**Trust every authenticated meeting peer, gated by host-side checks.** There is
no per-user allow-list. Anyone who is a legitimate participant in the room is a
potential controller of any window you're sharing, *while remote control is
enabled*.

## What IS enforced (host side)

- **Sender identity is authenticated, not spoofable.** The host overrides the
  packet's `controllerId` with the LiveKit-authenticated `trusted_sender`, so a
  peer cannot impersonate another controller (`remote_control.rs`, the
  `trusted_sender` path).
- **Requester must be a current room participant.** A control `Request` is
  rejected unless the requester is presently in the room's participant list
  (`RequestGate::RequesterNotPresent`). A packet from someone who has left, or
  who was never in the room, is dropped.
- **Host can disable, and disable revokes immediately.** "Disable for meeting"
  (`set_remote_control_allowed(false)`) calls `revoke_all` synchronously: every
  active controller is dropped and any held keys/buttons get synthetic release
  events **at the moment of disabling** — not lazily at the next input. There is
  no window where a controller keeps injecting after the host turns RC off
  (`session/commands.rs`, `remote_control::revoke_all`).
- **Concurrent active controllers per window.** A new controller gets its own
  grant and does not displace existing controllers, so multiple peers can
  control one window concurrently. Each controller's input and revocation
  state is tracked independently.
- **Revoke on share-stop and disconnect.** Stopping a share revokes every
  active controller; a controller leaving revokes only its own control. Both
  paths release the departing controller's held inputs.
- **Per-grant capability binding.** Each authorization gets a fresh 128-bit,
  lowercase-hex capability token. The host returns it only in the targeted
  `active` status, and the controller echoes it on pointer, wheel, key, text,
  and native clipboard packets. Rotation on re-grant and removal on revoke
  means stale or replayed control streams stop at the host. For a capable grant the
  key also includes `targetKind` and the opaque live `shareInstanceId`; a
  window/display mismatch, stopped/restarted share, partial envelope, or
  unknown target kind cannot reuse the grant. Legacy packets continue to use
  the historical `(windowId, controllerId)` key. Native and web-harness peers
  still retain a compatibility window in which tokenless legacy input is
  accepted with a warning (`remote_control.rs`'s legacy tokenless JSON path;
  `web-harness/src/remoteControl.ts`). It was meant to last one release and
  has outlived that; no removal release has been set. When it is dropped,
  record the version here.
- **Terminal results do not overclaim delivery.** `applied` is reserved for an
  observed semantic target operation. Global OS submission without an observed
  application effect reports `submitted` — which is also what a window-share
  wheel reports, since a successful `SendMessageTimeoutW` delivery proves only
  that the target window proc received the message, not an application effect.
  Failures carry only stable, privacy-safe route/reason codes; they do not
  echo input text, coordinates, window titles, process names, or raw OS errors.
- **Operation refusal does not revoke a Windows session.** Once a grant is
  active, occlusion, foreground, integrity, secure-field, unsupported-route,
  timeout, target-state, and other replay/resolve failures are operation
  outcomes only. Reliable operations return a correlated result; legacy
  high-rate input may receive throttled feedback. Native/web controllers show
  that warning briefly without deleting the grant or disabling forwarding.
  Sharer disable/revoke, controller release/disconnect, share teardown or
  replacement, and room teardown still end the session and release held input.
- **No automatic mode escalation.** Window-share wheel performs a single
  cursor-preserving `SendMessageTimeoutW` route with no fallback when it fails;
  other window routes keep their foreground/point/target gates. A request for
  full control requires a new request and sharer approval; one failed operation
  can never acquire stronger control.
- **Windows replay is target- and desktop-bound.** Window replay re-resolves the
  current capture token to its HWND/PID, rejects hidden/minimized/replaced or
  higher-integrity targets, requires the default input desktop, and rechecks
  foreground ownership around global pointer input. The window wheel delivers
  `WM_MOUSEWHEEL`/`WM_MOUSEHWHEEL` via `SendMessageTimeoutW` to the shared
  window's own SCROLLABLE descendant under the cursor (with the top-level
  window as fallback), so a covering window neither blocks nor redirects the
  message — ID-addressed injection is intentionally not occlusion-gated (the
  controller already aimed at a specific shared window); occlusion/covered
  checks apply only to the global-cursor pointer/keyboard routes. The wheel
  uses no cursor/focus APIs. Known Chromium behavior: at a point physically
  covered by another window on the sharer's desktop, the browser's render
  widget ignores the delivered wheel because Chromium reroutes wheel input to
  the window under the pointer (the covering window owns that point) — this
  is inherent to how Chromium routes wheel by screen position, not a delivery
  gap; uncovered points scroll normally and non-Chromium apps (e.g. Win11
  Notepad) scroll even when covered. Application class is not an admission
  rule. Keyboard/text additionally requires a focused element owned
  by the target and a trustworthy secure-field result; unknown UIA text
  providers fail closed. Win/Meta remains explicitly unsupported for a window
  share because it can drive the
  sharer's shell outside the shared surface.
- **Telepointer tags show the sharer's real cursor over the topmost window.**
  The cursor tag a controller sees on a remote window reflects where the
  SHARER's cursor actually is on their desktop, resolved by the topmost
  window at that point (`root_window_at`). At a point covered by another
  window on the sharer's desktop, the tag therefore lands on the covering
  window, not the shared window beneath it — the same z-order truth the
  occlusion gate uses. This is honest positioning, not a misplacement: the
  tag would be wrong if it claimed the cursor was over a window that is, on
  the sharer's desktop, underneath another window. A controller clicking a
  covered point is refused (the pointer occlusion gate) and the click's
  release never warps the sharer's cursor to the refused point, so the click
  is a true no-op and the tag does not jump on click.
- **Direction and reliability are pinned.** Controller input is accepted only
  from the LiveKit-authenticated controller identity; `status`/`result` packets
  are accepted only from the authenticated host and are targeted back to the
  controller. Reliable discrete input and terminal packets cannot silently
  switch to the lossy movement stream. The machine-readable policy is
  `contracts/petal-contracts.json.remoteControlPacketPolicy`.
- **Native clipboard is a separate one-way boundary extension.** A bare native
  Copy from a controlled application window means B→A; a bare native Paste
  means A→B. The Copy request and targeted plain-text stream require the active
  grant, exact application-window binding, operation correlation, and a
  nonempty valid UTF-8 body no larger than 1 MiB. Standard file-list/file-
  promise formats, files, NUL-containing text, and oversized data are rejected
  before any transfer. Host Paste updates the host clipboard before invoking
  the existing target-safe native Paste path and does not restore it. No
  clipboard text, fingerprint, or stream body is sent in status/result packets,
  logs, telemetry, or cockpit ledgers; the LiveKit SFU can still read the
  transport as it can read other remote-control data.
- **Keyboard clipboard operations are not B-local semantics.** Petal does not
  track clipboard origin or infer a Copy→Paste pair. A keyboard Copy→Paste may
  lose rich/native clipboard data and is not supported as a native or lossless
  B-local workflow. Users who need B-local behavior should use the target
  application's reachable in-window context menu, toolbar, or dropdown for
  both operations; mixing its Copy with Petal keyboard Paste overwrites B from
  A. Browser peers do not implement the native clipboard extension.

## What is NOT enforced (known gaps)

- **Strict viewer-only-of-that-window authorization.** The host verifies the
  requester is *in the room*, but not that they are actually *subscribed to /
  viewing that specific shared window*. Window IDs are sequential and guessable,
  so a participant could in principle request control of a window they aren't
  looking at. Closing this requires **LiveKit per-track subscription state**,
  which is not exposed to application code in the SDK version in use — it needs
  backend/protocol work (query the SFU's per-participant track-subscription
  list, or a signalling extension). The code flags this exact spot
  (`remote_control.rs`, the "local request gate accepted … strict viewer-only
  authorization" log line). **This is the one gap #30 left open; it needs SFU
  subscription state the SDK does not expose.**
- **No per-topic publish ACL.** Backend tokens grant `canPublishData` globally
  (`backend/lib/livekit.ts`), so any participant can publish on the
  `petal.remote-control` topic. The entire defense therefore rests on the
  host-side checks above; there is no transport-level scoping. Scoping this
  needs per-topic publish ACL support in the token grants (if/when LiveKit
  exposes it).

The grant token closes the stale/replayed control-stream gap and provides the
session-bound capability needed for future consent gating. It does **not** fix
the two transport/client gaps above: per-topic publish ACLs remain a backend/
LiveKit feature, and viewer-of-this-window authorization still requires
per-track subscription state that the current LiveKit SDK does not expose.

## Operational consequences (read before relying on this)

- **Invites are sensitive.** A leaked invite link lets someone join the room as
  a legitimate participant — and therefore become a potential controller the
  moment RC is on. Treat invite links like meeting passwords.
- **Consent is the default (`ask`).** Every host -- macOS and Windows -- runs a
  sharer-side policy of `off` / `ask` / `auto` (Settings > Privacy & Sharing,
  default `ask`; the per-meeting pill flips between `off` and that default).
  Under `ask`, an authenticated in-room request is PARKED: the controller gets
  `awaitingConsent`, the sharer gets a non-activating prompt ("<Name> wants to
  control <window>", Allow / Deny, visible countdown), and no grant token
  exists until an explicit Allow. The same panel also renders Windows
  full-control escalation prompts ("<Name> requested full control of <window>").
  No answer within 30 s denies ordinary consent or expires escalation
  (`denied` / `consentTimedOut` for ordinary consent); an escalation timeout
  never changes the mode. Deny, a share stop, the requester leaving, or the
  policy turning off deny too. Allow re-checks the gate at answer time and then
  runs the same authorize tail `auto` runs for ordinary consent. See
  docs/CONTRACTS.md "Sharer consent".
- **`auto` is the former legacy behavior, now opt-in.** An authenticated
  in-room requester is granted immediately. A Mac host on `auto` implicitly
  allows control by room peers the moment RC is on; choose it knowingly.
- **Windows capability negotiation is unchanged.** A capable Windows host
  still validates the v2 envelope BEFORE the consent prompt (a request that
  would be refused never bothers the sharer); a legacy controller talking to
  a capable Windows host receives the stable neutral `requestUnavailable`
  status with optional `controllerUpgradeRequired` metadata.

## Testing

- A packet whose sender is not a current participant is dropped
  (`RequestGate::RequesterNotPresent`).
- Disabling RC mid-session stops all active controllers with no timing window
  and releases held inputs (`revoke_all` + synthetic releases).
- The remote-control local-loopback harness exercises acquisition/latency
  without a second Mac; the strict viewer-only and per-topic-ACL gaps above are
  the parts that still need backend/live validation (both #28 and #30 are
  closed; the gaps remain and are tracked in this document).
- Rust and TypeScript contract tests pin legacy byte compatibility, the capable
  target/share fingerprint, unknown optional enum handling, result semantics,
  and the transport authority matrix. Portable fake-adapter tests exercise the
  platform and control-surface seams without calling OS injection.
