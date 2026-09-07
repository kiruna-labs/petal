---
title: Remote control
description: Letting someone else control a window you're sharing, and the trust model behind it.
---

Remote control lets a teammate drive a window you're sharing: their mouse
clicks, scrolling, and keyboard input are injected into that window on your
machine, as if they were sitting at your keyboard for that one window.

## Asking for control

From the [shared window's header](/docs/using/viewing-shared-windows/) on
the viewer's side, switch the mode segment to **Control**. What happens next
depends on the sharer's policy (below). Once control is granted, your input
drives the window until you switch back to **View**, the share stops, either
of you leaves the meeting, or the sharer turns remote control off.

The **Control** segment is only shown for windows that can be controlled at
all. A browser participant's share can never be controlled (a browser can't
inject input into an operating system), and a sharer whose policy is **Off**
advertises that too, so you won't see a button that can only fail.

More than one person can control the same window at the same time. A new
request doesn't bump the current controller — it adds another one. All of
their inputs are live on the window until each of them stops, or the sharer
turns remote control off (which drops all of them at once).

## The sharer decides: Ask, Allow automatically, or Off

Control of your shared windows is governed by one setting,
**Settings → Privacy & Sharing → Remote control of my shared windows**:

- **Ask me each time** (the default). Every request raises a small consent
  panel on your screen — "*Name* wants to control *window*" with **Allow** and
  **Deny**. The panel never steals focus from what you're doing. If you
  don't answer within 30 seconds the request is denied. Further requests
  queue behind it ("One more request is waiting.").
- **Allow automatically.** Anyone in the meeting can take control of any
  window you share, immediately, with no prompt. Use this for a trusted
  pairing session where the prompts would just get in the way.
- **Off.** Requests are refused.

Two more controls sit beside that setting:

- The meeting bar's **More** menu has **Disable remote control** /
  **Allow remote control**, which turns it off (or back on) for just the
  current call without changing your default.
- Each shared window's hover-tab menu has its own **Allow remote control**
  checkbox. Both gates must allow control — the per-window lock can refuse a
  window even when your policy is **Allow automatically**.

Turning control off at any level immediately drops every teammate who
currently has control and releases any keys or mouse buttons they were
holding down — there's no delay or lingering window where they can keep
sending input.

One macOS-level gate also applies on your side: injecting input requires
Petal to have **Accessibility** permission (System Settings → Privacy &
Security → Accessibility). Petal asks for it during setup; without it every
control request is denied and the requester sees an error.

While someone is controlling a window you share, the signals you get are the
window itself reacting, the controller's colored telepointer moving over it,
and — under **Ask me each time** — the fact that you approved it. If you
don't want a window controllable at all, uncheck **Allow remote control** in
its hover-tab menu before sharing, rather than relying on noticing.

## What Petal enforces

- Whoever is controlling you is a real, authenticated participant in the
  room — nobody can spoof being a different teammate.
- A request from someone who has already left the room, or was never in it,
  is rejected.
- Stopping a share, or the controller leaving the meeting, immediately ends
  their control.
- A grant is bound to one specific share: if you stop and re-share the same
  window, the old grant is gone.

What Petal does **not** enforce: that the person requesting control of a
specific window is actually looking at that window on their own screen. It
checks that they're a legitimate participant, not that they have the window
open. Treat meeting invite links accordingly — anyone who joins the room can
ask to control anything you share, for as long as remote control stays on.
The full threat model is written up in the repository's
`docs/remote-control-trust-model.md`.

## What remote control can't do

Input is delivered through macOS's accessibility APIs (with a fallback that
posts events directly to the target app's process), aimed at the shared
window and the app that owns it. Pointer positions are clamped to the shared
window's bounds. It isn't a hardware-level takeover of your whole machine —
a controller can't move your real cursor around the desktop or drive other
apps.

The flip side: within that window, the input is real and has real side
effects. If you share a terminal, a controller can run commands; if you share
a browser window, they can navigate and type anywhere that window can. Treat
granting control of a window as granting what that window can do.

Some elements in some apps may not respond the way they would to a real click
or keypress, especially unusual custom UI that doesn't expose itself through
accessibility.

## Control modes on Windows

On Windows, a shared window starts in **Cursor-preserving** mode: the
controller's clicks land in the window without moving your real cursor. A
controller who needs continuous pointer input can request **Full control**;
that request appears in the same consent panel ("They already have
cursor-preserving control…") with **Allow** and **Deny**, expires after 30
seconds, and always fails closed. You can also pick the mode per share from
the hover-tab menu's **Remote control** section. Petal never escalates on its
own.

## Clipboard shortcuts

Between two desktop apps, the bare keyboard shortcuts have fixed semantics
while you control a shared window:

- **Copy** (`Cmd+C` on macOS, `Ctrl+C` on Windows) copies plain text from the
  shared window on the sharer's computer to your computer.
- **Paste** (`Cmd+V` / `Ctrl+V`) sends your current plain text to the
  sharer's computer, replaces its clipboard, and pastes into the shared
  window.

Only non-empty plain UTF-8 text up to 1 MiB is transferred. Files, file
lists, images, and other rich clipboard formats are not. Text that happens to
look like a path is still ordinary text.

These shortcuts are **not** a way to copy and paste only within the sharer's
computer. A Copy followed by a Paste passes eligible plain text through your
clipboard and can lose formatting, so Petal does not treat it as lossless. If
you need a copy/paste that stays on the sharer's machine, use the shared
application's own context menu or toolbar for **both** operations, when that
UI is reachable inside the shared window. Browser participants do not
implement these clipboard streams.

## Local echo (experimental)

If you're the one controlling someone else's window, **Settings → Privacy &
Sharing → Local echo (experimental)** controls whether you see instant
feedback for your own input — a ripple for clicks, a pending-text strip while
typing — before the real updated frame comes back from the sharer's machine.
It's off by default. When on, what you see is a prediction of what you just
sent, not confirmation that it happened; the real frame is still the source
of truth. It only affects your own view while you're controlling.
