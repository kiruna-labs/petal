# Windows native-surface consistency audit

> Scope: Petal's Windows `WebviewWindow`/Win32 surfaces involved in sharing,
> placement, receiving, and diagnostics. This is an inventory and follow-up
> ledger, not a license to bundle unrelated native-window changes into the
> share-indicator fix.
>
> Status: the capture-indicator and Petal View changes are implemented in the
> current worktree. Rows marked **unverified** require a real packaged/release
> run; this Windows host cannot provide the macOS half of the placement matrix.

## Policy matrix

| Surface | Shape / corner owner | Shadow | Transparency | Activation / focus | Capture affinity | Hit-test policy | DPI / monitor policy | Live coverage |
|---|---|---|---|---|---|---|---|---|
| `main` | Gallery mode uses native DWM rounding; pill mode uses transparent CSS shell | CSS/token shadow where applicable; no native shadow in pill mode | Mode-dependent | Main startup/explicit navigation may activate; passive updates do not | N/A for local app chrome | Main route owns its controls; pill mode has native shell hit-testing | Tauri monitor scale; logical frontend geometry | Existing Windows release smoke; mixed-DPI follow-up |
| `hover-tab` | Transparent CSS fixed 40×40 rail; no DWM corner dependency | Token pill shadow | Transparent WebView2 | Ordinary sources use non-topmost source-relative WinEvent/native placement; elevated or integrity-unknown sources use a temporary `HWND_TOPMOST` fallback with `SWP_NOACTIVATE`; explicit controls may focus | N/A | Pill + bridge only; compact button is both Share/Stop and the 6px-threshold drag surface | One DWM-visible physical source frame plus current HWND DPI; persisted/previewed normalized vertical offset projects into `rcWork`, preserves outside/inset attachment when possible, uses immediate source-relative insertion for ordinary sources, and hit-tests through the temporary topmost fallback for elevated sources; a foreground or actively shared elevated source keeps its cached tab over underlying windows, while another source follows the normal hit-test retarget/hide path; hides fail-closed when no safe 40×40 rectangle or native placement exists | Focused geometry/token/work-area/gesture/menu tests; owned-PID Share/Stop and optional position/priority/occlusion smoke |
| `window-picker` | Rectangular WebView with native DWM rounded treatment on Windows | Native/configured picker treatment | Transparent shell with opaque content | Created/revealed without focus | N/A | Picker cards and controls own input | Centered on the monitor/work area selected by the main window | Existing picker/rendered tests; release check pending |
| `network-cockpit` | CSS-owned secondary window shape; native DWM policy is not unified with picker | CSS/token panel shadow | Transparent WebView shell | Explicitly opened diagnostics surface | N/A | Full diagnostics controls | Tauri scale and current monitor | Source/rendered tests; native corner parity unverified |
| `region-window-*` | Two-band CSS frame: outer/inner 3px strokes, 16px outer radius | No external shadow | Transparent | Placement is no-activate; title drag/control is explicit | Windows uses `WDA_NONE` while idle and a label-owned `WDA_EXCLUDEFROMCAPTURE` lease only during an active display-region capture; acquisition failure retains WGC's system indicator | Entire frame is opaque during placement; after release only border/title/edge zones stay hit-testable; registered selector rectangles block hover tabs through the hollow interior | Native position/size events plus ResizeObserver; physical Windows frame and logical macOS frame | Dual-UA rendered placement probe; affinity lifecycle contracts; real Windows click pending; macOS live unavailable |
| `petal-sharer-pointer-*` | Transparent overlay; optional inset CSS 4px identity stroke with 10px radius | None | Transparent | Native `SWP_NOACTIVATE`; Draw intentionally focuses only when enabled | Required for display/region overlays; failure hides overlay and keeps WGC system indicator | `pointer-events:none` except intentional Draw mode | The same DWM-visible physical source snapshot drives the border and hover tab; verified same/lower-integrity sources use a Win32 owner relationship, while elevated or integrity-unknown sources use an unowned passive telepointer and keep WGC's system border; ordinary updates use `SWP_NOZORDER`, and only display overlays use topmost placement; one WinEvent pump updates all followers with a 250ms reconciliation timer | WGC window probe passes; deterministic placement/action tests, owner lifecycle test, and opt-in real WinEvent smoke are available; live continuous-follow metrics pending |
| `petal-remote-*` surface | Remote WebView plus native video child; DWM native corner owner | Native/WebView surface policy | Transparent shell; video child owns content | Reveal is passive; activate command intentionally focuses | N/A to received content | Surface/header controls; video child receives content input | Compositor owns physical child geometry and source-size updates | Existing Windows compositor tests; cross-DPI live coverage partial |
| `petal-control-*` | Transparent sibling input overlay; no visual frame | None | Transparent | Remains passive while receiving control events | N/A | Interactive only for authenticated remote-control mode | Compositor synchronizes sibling overlay to video content | Existing remote-control/source tests; live effect matrix separate |
| `petal-pointer-*` | Transparent sibling telepointer/Draw overlay; CSS pointer tags | Token/transient pointer effects | Transparent | Passive click-through | N/A | `pointer-events:none` unless Draw is active | Compositor tracks video content rect | Existing pointer/draw tests; multi-monitor live coverage partial |
| `ai-chat-panel` | CSS panel shape in a transparent Windows `WebviewWindow` | CSS/token panel shadow | Transparent shell | Present is no-activate; explicit panel controls can focus | N/A | Panel controls only | Clamped beside the shared window's work area | Source tests; native Windows visual coverage unverified |
| `control-consent` | CSS consent card in a transparent hidden singleton `WebviewWindow` | CSS/token panel shadow | Transparent shell | Presented with `SWP_NOACTIVATE`; Allow/Deny remain interactive | N/A | Card buttons only | Top-center of the cursor monitor's work area; logical content size with physical placement | Queue/timeout and native wiring tests; live second-peer focus check pending |
| `dev-telepointer` (when enabled) | macOS/Windows debug `WebviewWindow`; not a customer share indicator | Debug-only | Debug-only | Debug harness policy | N/A | Debug harness controls | Test harness-owned | Debug-only; not release scope |

**Excluded from the Windows rows:** macOS-only `share-border`, `share-bar-*`,
`menubar-popover`, and `NSPanel` compositor surfaces; they remain documented in
the cross-platform inventory but have no Windows native instance. A WGC system
border is not a Petal window/surface: it is OS-owned capture chrome and is the
safe fallback when the custom replacement is not proven.

## Decisions locked by this work

1. `GraphicsCaptureAccess.RequestAccessAsync(Borderless)` is requested once per
   process, only when a real share starts. `Allowed` is necessary but not
   sufficient for a custom indicator.
2. `IsBorderRequired(false)` is selected only after a local replacement is
   shown. A full-display overlay must also pass capture exclusion; a Petal
   View selector acquires its own exclusion immediately before WGC starts and
   releases it after capture teardown. If the required exclusion fails, the
   custom path is not selected and WGC keeps its system border. Idle selectors
   remain `WDA_NONE` and therefore recordable.
3. Petal View does not receive a second sharer border. Its own neutral/tinted
   two-band frame is the replacement surface.
4. The Windows sharer overlay is one surface per share and owns both
   telepointers/Draw and the optional custom border. The idle hover tab joins
   this same WinEvent/message-pump follower, so both surfaces consume one
   DWM-visible physical source snapshot and the tab uses the source HWND's
   current DPI projection. Same/lower-integrity ordinary windows use a verified
   Win32 owner; elevated or integrity-unknown sources use only a passive
   unowned telepointer and keep WGC's system border. The hover tab is
   non-topmost and inserts immediately above its source in the source's
   normal/topmost band for ordinary sources; an unrelated window above that
   source therefore occludes the tab naturally. Elevated or integrity-unknown
   sources use a temporary `HWND_TOPMOST` fallback, and hover hit-testing walks
   through that tab; while the elevated source remains foreground or actively
   shared, its cached tab stays attached even if another window is underneath.
   Otherwise the normal hit-test path retargets or hides it. Its own reorder
   event is ignored.
   Admission combines the
   event object/child fields, active source/follower context, and the
   `GetAncestor(hwnd, GA_ROOT) == hwnd` top-level check for reorder events; GA_ROOT
   equality alone is not a general proof that browser/render/details-pane churn
   is harmless. Accepted foreground/top-level reorder events queue the
   coalesced tracker, which performs the full reconciliation. Ordinary sharer-overlay
   geometry/show/hide updates use `SWP_NOZORDER`; only display overlays use
   topmost placement. The 250ms timer is only a missed-event/display safety net.
   If a live replacement
   becomes untrustworthy, the capture thread first restores WGC's system border
   and only then hides Petal chrome. It is cleaned
   up on start failure, first-frame timeout,
   publication failure, room-commit race, stop, source loss, leave, and
   selector close.
5. Opaque WGC tokens remain disposable. Stop invalidates them; the selector
   registry rebinds by stable Tauri label/title and never revives a stale token.
6. Petal View owns its only Share/Stop control in the persistent title bar. Its
   full selector rectangle blocks ordinary hover-tab targeting, while ordinary
   window hover tabs retain their existing behavior.
7. The `control-consent` route is a dedicated singleton on both desktop
   platforms. Windows uses a hidden always-on-top WebviewWindow and
   `SWP_NOACTIVATE`; queueing and timeout-deny remain backend-authoritative.
8. The Windows hover action is the only marked exception to the global
   WebView2 title suppression: its concise Share/Stop title includes “drag to
   move” and “right-click for options,” while its full context remains in
   `aria-label`. Right-click, Shift+F10, and Context Menu use the existing
   Tauri system-native menu; Top/Center/Bottom are hover-only entries and no
   custom popup is permitted.
9. The hover tab stores one normalized app-wide vertical offset in
   `share-preferences.json`. Pointer previews are memory-only and commit on
   pointer-up or a position preset. The follower freezes during the drag and
   cancels on stale target, source loss, room leave, Escape, or lost capture.

## Ranked follow-up findings

### P0 — complete real display-pixel evidence

Run a release/NSIS build with a second peer, share a full display, capture the
local monitor and the peer's raw received frame, then run
`scripts/windows-display-indicator-smoke.ps1`. The local edge must contain the
identity stroke while the received frame must not. This cannot be proven by a
DOM screenshot or a WGC API return value alone.

### P1 — packaged/unpackaged borderless capability confirmation

The Microsoft API requires the `graphicsCaptureWithoutBorder` package
capability for `RequestAccessAsync(Borderless)`. Petal's shipped Windows lane
is an unpackaged NSIS executable, and this host records a `NonPackaged` allow
entry. Verify the installed NSIS executable's returned status and consent
prompt exactly once. If it returns `NotDeclaredByApp`, retain the system border
and document that the custom path is unavailable for that install channel;
do not weaken the fallback.

### P1 — display-affinity variability

`WDA_EXCLUDEFROMCAPTURE` is treated as a hard prerequisite for display/region
custom chrome. Collect results across Windows 10/11, mixed-DPI monitors,
remote-desktop sessions, and GPU/VM environments. A failure should remain a
safe system-indicator result, not a new capture error.

### P1 — local NSIS architecture gate mismatch

A fresh local `Petal_0.9.4_x64-setup.exe` was produced, but its PE header is
`0x014c` while the bundled `desktop.exe` is `0x8664`; the existing release gate
expects the setup executable itself to be `0x8664`. Resolve this release-tooling
question (or change the gate to validate the payload deliberately) separately;
it is not part of the capture-indicator change and the installed package was
not used as runtime evidence here.

### P2 — native corner ownership is still mixed

The main/picker/remote receiver use DWM-native rounding while transparent
selector and local sharer surfaces use CSS clipping. Audit fractional-DPI edge
pixels and Windows 10 square-corner behavior separately; do not replace the
selector's tested two-band frame as part of that follow-up.

### P2 — diagnostics/AI panel focus and shadows

`network-cockpit` and `ai-chat-panel` have CSS-owned visual chrome and distinct
present/focus behavior. Compare their no-activate, shadow, and close semantics
with the picker and remote receiver in a dedicated surface pass.

### P2 — monitor migration stress

Exercise every row that follows a source across monitor removal, DPI changes,
work-area changes, minimize/restore, and negative virtual-screen coordinates.
The share overlay and Petal View already use physical/native geometry where
required, but the cross-surface matrix lacks a single live stress run.

## Verification record

- `bun run check`: passed with 0 errors and 0 warnings; the full frontend
  suite passed (727 tests, including the focused hover, work-area, tooltip,
  Petal View, indicator, and consent coverage), and `bun run build` completed.
- Rendered Petal View dual-UA fixture at 640×400 and 160×120: passed,
  including pending Share, idle/shared palettes, placement stale-event
  filtering, mouse-up restoration, title wrapping, three full-size title
  actions, and no horizontal overflow.
- Focused Windows policy, visible-frame, overlay action, selector registry,
  hover-blocker, options-menu, Petal View controls, consent, and WGC tests
  passed. The overlay suite now has 12 passing tests, including state-diff
  coverage for 1px movement, maximize-like resize, hidden states,
  foreground stacking, and self-anchor rejection.
- The opt-in real WinEvent tracker smoke passed on this interactive Windows
  desktop for synthetic move, maximize, restore, foreground activation, and
  visible-frame convergence.
- Brave/Explorer client-click compositor flashing remains unresolved. Headless
  event-admission tests cannot validate DWM/WebView2 occlusion, and no live
  compositor result is claimed.
- Release `windows_share_source_probe` live WGC session passed with 29 frames
  in 10 seconds against the Adobe Premiere window; this proves release WGC
  frame delivery, not the custom indicator path.
- `cargo check --locked`, `cargo build --locked`, touched-file `rustfmt --check`,
  PowerShell 5.1/7 parsing, embedded C# compilation, and `git diff --check`
  passed. The full Rust library suite reached 1,148 passed, 7 ignored, and one
  known pre-existing failure in
  `session::tests::non_empty_audio_opt_out_disables_wasapi`.
- The OBS-affinity lifecycle amendment has focused source contracts covering
  idle creation, pre-WGC acquisition, failed-start rollback, capture-drop
  ordering, Stop/autonomous failure, leave/close cleanup, token rebinding, and
  per-selector isolation; those contracts and the Windows `cargo check`
  passed. A bounded `GetWindowDisplayAffinity` probe found no Petal top-level
  window to inspect on this run. Live affinity/OBS idle → active → idle and
  local-vs-received-pixel checks therefore remain unavailable without an
  active Petal share and peer, so no interactive recording result is claimed.
- All four Windows smoke scripts parse in PowerShell 5.1 and 7, and the
  embedded hover-tab C# helper compiles. The
  `windows-hover-tab-smoke.ps1 -ExerciseFollow` path includes a deliberate
  tab-offset positive control, 8ms source/border/tab sampling, a red
  `border-current/tab-previous` assertion, and the taskbar-edge
  `Screen.WorkingArea` exercise; it requires an explicitly owned Petal PID and
  an active share. The dedicated Windows consent panel also passes native
  wiring/height checks; its live no-focus-steal and second-peer request flow
  remain pending. The red-capable
  `windows-share-overlay-tracking-smoke.ps1` was run before and after the code
  change, but both runs correctly stopped because no active ordinary-window
  share with a `Petal Sharer Pointer` overlay was present. The hover-tab smoke
  gate was also run with a non-Petal PID and correctly refused to start because
  no Petal binary was running. Therefore the owned-PID continuous-follow,
  taskbar-edge, Share→Stop→Share→Stop, and live edge/visibility metrics remain
  a required manual follow-up; no interactive result is claimed. The
  child-reorder extension was parser/C#-compiled and attempted with an owned
  Petal PID, but stopped before the child phase because no active hover
  tab/meeting was observable; no live child-reorder gap or zero-gap result is
  claimed. No p50/p95/max live tracking, edge-error, visibility-gap, z-order-gap,
  first-paint delta is claimed yet. The placement script also correctly
  reported that no follow-cursor selector was active; a synthetic 1x1
  display-frame check correctly selected the no-Petal/system-fallback failure
  branch.
- The movable rail adds normalized geometry, preference, pointer-threshold,
  native-menu dispatch, Windows projection, macOS parity, and source-loss
  cancellation contracts. The Windows smoke now has opt-in
  `-ExercisePosition`/`-ExerciseShare` quality and position-preset checks that
  record `share-preferences.json` and native geometry; no owned-PID interactive
  result is claimed on this run.
- Release cargo binaries were rebuilt. Installed NSIS consent/indicator
  behavior remains unverified; the existing setup-header/payload architecture
  mismatch is recorded above. Real `SendInput` Petal View placement and
  local-vs-received display pixel checks require an interactive meeting and a
  second peer and are not claimed as executed.
- Live macOS placement verification remains unavailable on this Windows host.
