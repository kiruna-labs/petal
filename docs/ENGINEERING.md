# Engineering notes

Hard-won constraints that are easy to reintroduce and unpleasant to rediscover.
**Read the crash classes before touching native window code.**

These are described in terms of the failure they cause rather than the history
behind them; issue numbers are kept where they anchor a specific fix.

## Crash classes — read before touching native window code

1. **AppKit off the main thread.** Building/closing any `NSPanel`/`NSWindow`
   (`PanelBuilder::build`, `window.close()`, layer attach) from a background
   thread traps (`EXC_BREAKPOINT` / "Must only be used from the main thread").
   Any code that creates/closes a native window from an async command or spawned
   thread MUST wrap the AppKit work in `platform::on_main(...)` /
   `app.run_on_main_thread(move || { … })`.
2. **Never `window.close()` a `tauri_nspanel` panel — hide + retire + reuse.**
   Destroying one (even main-thread, children-closed-first, `releasedWhenClosed=
   NO`) aborts seconds later via an ObjC exception in deferred dealloc that
   `objc2::exception::catch` cannot catch. Every dynamic panel needs a
   hide-and-retire lifecycle (compositor `CompositorState::retired`,
   `share_border`). Tests must exercise the REAL UI path
   (`hover_tab::toggle_share_for_window`), not just the session layer.
3. **livekit SDK calls need an ambient tokio runtime.** Even "sync-looking"
   methods (`LocalAudioTrack::mute()/unmute()`) internally `tokio::spawn` and
   panic-abort ("no reactor running") when called on the main thread (menubar
   clicks, sync commands in wry's URL-scheme handler). Wrap in
   `tauri::async_runtime::block_on` or make the command async. Agent launches
   set `PETAL_DISABLE_AUDIO=1` (skips mic code) — audio changes need one
   audio-ENABLED validation run.
4. **`CGEventPostToPid` has no real effect for pointer/drag/scroll — only
   keyboard.** Confirmed live (2026-07-05, see the project history):
   keyboard routing is responder-based, but pointer/drag/scroll routing
   depends on the real ambient system cursor position, which
   `CGEventPostToPid` (posting directly into a process's event queue,
   bypassing WindowServer/HID) never updates. Remote-control input replay
   (`remote_control.rs`'s `mod input`) uses Accessibility-API direct
   manipulation instead (`AXPress`/`AXSelectedTextRange`/scrollbar `AXValue`/
   `AXShowMenu`), falling through to the old CGEvent path only as a last
   resort for content with no AX affordance. If you ever need a NEW native
   input-injection path, don't reach for bare `CGEventPostToPid` for anything
   but keyboard — it will silently do nothing.
5. **Continuous `AVSampleBufferDisplayLayer` enqueue to a sleeping display
   risks an OS WindowServer watchdog kill.** Confirmed live (2026-07-08, see
   #264): a real user's crash log showed **WindowServer** (not Petal) killed
   by the OS watchdog ("Display not ready", `displayState` OFF) while Petal
   was idle but had recently been driving ~30fps compositor enqueues to a
   receiver whose display had gone to sleep. Fix (#259/#264): `platform::
   power::DisplaySleepAssertion` (IOKit `IOPMAssertionCreateWithName`/
   `kIOPMAssertionTypePreventUserIdleDisplaySleep`) is held for the duration
   of `session::room::join_room`..`leave_room` to prevent *idle* display
   sleep during a meeting, and — the real safety net, since that assertion
   cannot stop a user-forced sleep (lid close, `pmset displaysleepnow`) —
   `resilience.rs`'s `screensDidSleep`/`screensDidWake` `NSWorkspace`
   observers call `compositor::set_display_enqueue_paused(true/false)`,
   which gates `compositor::push_frame` so it stops handing frames to
   `AVSampleBufferDisplayLayer.enqueueSampleBuffer:` for the whole time the
   display is confirmed asleep. The pause is enqueue-only — it never stops
   `transport::subscriber`'s LiveKit decode loop, so the local decoder's
   reference-frame chain is never interrupted and no keyframe request is
   needed on resume (confirmed there is no public LiveKit 0.7 force-keyframe/
   PLI API to make one with anyway — see `session/share.rs`'s pump-decision
   log line referencing #182). If you ever add a second AppKit surface that
   commits frames to a display (a new compositor-like layer, a second
   display-driving path), it needs the same pause/resume wiring — don't
   assume the `IOPMAssertion` alone is sufficient.

## Windows native pitfalls

- One dedicated thread per session owns all D3D11/WGC/COM state (capture
  thread enters COM MTA; compositor thread runs the Win32 message loop).
  Cross-thread D3D11/WGC calls are bugs.
- `DXGI_ERROR_DEVICE_REMOVED`/`DEVICE_RESET` must recreate the device, swap
  chains, and textures and re-present the last stored frame — windows never
  die from device loss.
- WGC delivers frames only on content change; the share pump re-pushes the
  last frame on an idle timer so receivers don't stall on static content.

Read each module's own doc comment before touching that code.

## UI text must NEVER truncate (hard rule)

All user-facing copy — button labels, placeholders, titles, tooltips, room
names, statuses — must ALWAYS be fully visible. Text that is clipped, cut off,
ellipsized-by-accident, or overflowing its container is never acceptable.

When you add or change any user-facing string, make it fit **autonomously**:
1. Verify it fits at the real window width (main window is 400px wide; the
   create row shares width with the "Create/Join" button). When unsure, MEASURE
   — render the real element/font and check `scrollWidth <= clientWidth` (the
   Albert Sans placeholder-fit check that sized `.join-input` to 11px is the
   pattern).
2. If it doesn't fit: shrink the font, tighten neighboring elements (e.g. a
   more compact button), allow wrapping, or restructure — whatever keeps the
   FULL text visible with margin. Prefer keeping the approved layout; drop font
   before you drop the design.
3. If genuinely ambiguous (e.g. only a tiny/unreadable font would fit, or a
   layout change is needed), ASK rather than ship truncated text.

Never ship truncated/overflowing text, ever.

## Native window-lifecycle changes need a live-exercising test, not just unit tests

Three issues in a row targeting small-remote-window UX (#376, #466, #497) each
shipped green and still needed a follow-up pass — #497's own filed text called
this out explicitly ("this class of bug has now shipped twice while green")
before it happened a THIRD time in the same session that finally fixed it:
810 passing tests didn't catch a real showstopper (a pre-existing aspect-lock
`WindowEvent::Resized` handler silently fighting and undoing a new collapse
feature) because the tests only exercised pure helper functions
(`aspect_locked_content_height`, `remote_window_min_size`) in isolation, never
the actual native window-event handler chain those functions feed into.

**The pattern, not just the one bug:** a unit test on an extracted pure
function proves the function is correct given its inputs — it does not prove
anything is actually calling that function with the right inputs, in the
right order, from the real event/lifecycle path a user will trigger. Native
window state machines (resize, retire/reveal, chrome z-order -- Collapse
itself was removed in #675, precisely because it kept landing in this class
of defect) are exactly where this gap bites, because the interesting bugs
live in the *wiring*, not the arithmetic.

**Before merging any change to native window lifecycle/state (resize,
retire, z-order, panel show/hide):**
1. Either add a test that drives the real event handler/command path (not
   just the pure function it delegates to), or
2. Get an adversarial second-opinion review explicitly asked to find gaps
   between "the pure logic is right" and "the real event chain actually
   exercises it" — this is what caught the #497 showstopper; don't skip it
   as a formality once tests are green.
Green unit tests on isolated helpers are not sufficient evidence for this
class of change, full stop.

**FIRST STEP for any geometry/lifecycle bug: `PETAL_TRACE_PANEL_GEOMETRY=1`.**
It emits one ordered line per geometry write **and per refused write**, with
call site, gesture bit, and requested frame. Off, it costs one relaxed atomic
load. Turn it on before theorising — five fixes to this class (#376, #466,
#497, #465, #416) shipped green and failed live, and the single thing missing
every time was any record of **which writer actually moved the window**.

It is what finally cracked #416 on the sixth attempt, and the trace makes the
mechanism unarguable rather than inferred:

```
seq=39 reason=drag                       w=925.00  gesture=active
seq=41 reason=drag                       w=884.00  gesture=idle   <- bit lost
seq=48 reason=programmatic-source-driven w=720.00  gesture=idle   <- guard opens
seq=50 reason=drag                       w=867.00  gesture=idle   <- drag yanks back
```

**The lesson that generalises beyond one bug:** #416's guard was *correct*, and
the state it read did not survive the **window lifecycle**. A source republish
retires the remote window and re-reveals it from the reuse pool mid-gesture,
and a revealed `CompositorWindow` is built with
`user_resize_active: AtomicBool::new(false)` (grep for it in `compositor.rs`;
there are two construction sites) — so the
gesture bit is gone and the guard correctly concludes "no gesture in progress"
while the user's pointer is still down. When a guard looks right but behaves
wrong here, **suspect retire/reveal before suspecting the guard's logic.**

Note also why the exhaustive harness missed it: it holds **one**
`CompositorWindow` for the whole run and never retires or reveals it. Coverage
of an interleaving space says nothing about a lifecycle the model does not
contain. And beware sparse sampling — #416's "3/16 failure rate" was the
sampler's *detection* rate; a 40ms gap-free sampler found the defect in
**16/16**, including a 198pt excursion inside 137ms on a trial scored `pass`.

## Known deviations / flagged items

- **Glass-to-glass latency is the product's defining metric, and it is
  currently over target.** It decides whether remote control feels direct or
  sluggish. The last real measurement was taken on a healthy local path and
  came in meaningfully above the target; nothing has re-measured it since,
  despite several changes to the pointer route, publication reconciliation and
  the retire/reveal path landing afterwards. Treat any latency figure you find
  in an old document as stale, and **measure rather than assert** — see
  `docs/VALIDATION.md`. Frame rate is not the lever here; 30fps is adequate for
  this product's content (see #383 for why the 60 in `ShareQuality::Full` is
  cosmetic rather than a performance gap).
- **`--text-primary` in `tokens.css`** is a placeholder (`#F5F6F7`); the exact
  value was never extracted from the source design. Confirm before ship.
- **Fonts are variable-weight subsets, not static weights.** The shipped
  `manrope-variable.woff2` is a single variable family declared with one
  `@font-face` at `font-weight: 200 800`. All four families
  (Manrope, Albert Sans, JetBrains Mono, Fredoka) are SIL OFL-1.1. Only
  Manrope ships from `apps/desktop/src/assets/fonts/` (with its `OFL.txt`);
  the other three come in as `@fontsource/*` npm packages (see
  `apps/desktop/package.json`) imported from `src/routes/+layout.svelte`, and
  carry their own license files.
- **Camera-off meeting tiles intentionally center the participant name** (#137).
  This overrides the older canvas note that asked for a plain bottom-left chip
  only: camera-off tiles show a centered full name when it fits, otherwise the
  first grapheme; the bottom-left chip remains for video-on/no-stream states.
- **Scaffold used SvelteKit** (official `npm create tauri-app -t svelte-ts`), not
  bare Vite — still Svelte+TS+Vite, just file-based routing + `adapter-static`
  SPA. `src/routes/+layout.svelte` is the app entry for global CSS/font imports.
