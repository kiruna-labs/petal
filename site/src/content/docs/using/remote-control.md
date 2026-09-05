---
title: Remote control
description: Letting someone else control a window you're sharing, and the trust model behind it.
---

Remote control lets a teammate drive a window you're sharing: their mouse clicks, scrolling, and keyboard input get injected into that window on your Mac, as if they were sitting at your keyboard for that one window.

## Turning it on for a window

From the [shared window's header](/docs/using/viewing-shared-windows/) on the viewer's side, switch to the **Control** mode segment to request control of that window. Once control is granted, that person's input drives the window until they switch back to View, the share stops, they leave the meeting, or you turn remote control off.

More than one person can control the same window at the same time. A new control request does not bump the current controller — it adds another one. If several teammates request control of one of your windows, all of their inputs are live on it simultaneously until each of them stops, or you turn remote control off (which drops all of them at once).

## The consent model — read this before relying on it

There is no per-request approval popup. Petal does not ask you to click "allow" each time someone requests control of a window you're sharing. Instead, control is governed entirely by one setting:

**Settings → Privacy & Sharing → "Allow teammates to control my shared windows."** This is **on by default**. As long as it's on, any current participant in the meeting can request and immediately receive control of any window you're sharing — there's no additional prompt.

Turn this setting off (or use the in-meeting control to disable it for just that call) and control requests are refused until you turn it back on. Turning it off also immediately drops any teammate who currently has control and releases any keys or mouse buttons they were holding down — there's no delay or lingering window where they can keep sending input.

One macOS-level gate also applies on your side: injecting input requires Petal to have **Accessibility** permission (System Settings → Privacy & Security → Accessibility). The first time someone tries to control a window you're sharing, macOS prompts you to grant it; until you do, every control request is denied and the requester sees an error.

Petal does not notify you when someone starts controlling a window you're sharing. The signals you get are the window itself reacting and the controller's colored telepointer cursor moving over it. If you don't want a window controllable, turn remote control off before sharing rather than relying on noticing.

What Petal *does* enforce even while remote control is on:
- Whoever is controlling you is a real, authenticated participant in the room — nobody can spoof being a different teammate.
- A control request from someone who has already left the room, or was never in it, is rejected.
- Stopping a share, or the controller leaving the meeting, immediately ends their control.

What Petal does **not** currently enforce: that the person requesting control of a specific window is actually looking at that window. It only checks that they're a legitimate participant in the meeting — not that they're the one currently viewing the particular window they're asking to control. In practice this means being in a meeting with remote control turned on means any participant could request control of any of your shared windows, whether or not they have that window open on their own screen.

Treat meeting invite links accordingly — anyone who joins the room becomes a potential controller of anything you share, for as long as remote control stays on.

## What remote control can't do

Input is delivered through macOS's accessibility APIs (with a fallback that posts events directly to the target app's process), aimed at the shared window and the app that owns it. Pointer positions are clamped to the shared window's bounds. It isn't a hardware-level keyboard/mouse takeover of your whole Mac — a controller can't move your real cursor around the desktop or drive other apps.

The flip side: within that window, the input is real and has real side effects. If you share a terminal, a controller can run commands; if you share a browser window, they can navigate and type anywhere that window can. Treat granting control of a window as granting what that window can do.

Some elements in some apps may not respond the way they would to a real click or keypress, especially unusual custom UI that doesn't expose itself through accessibility.

## Clipboard shortcuts

On native desktop clients, the bare keyboard shortcuts have fixed cross-system
semantics while you control a shared application window:

- **Copy** (`Cmd+C` on macOS, `Ctrl+C` on Windows) copies plain text from the
  shared window on the sharer's computer to your computer.
- **Paste** (`Cmd+V` on macOS, `Ctrl+V` on Windows) sends your current plain
  text to the sharer's computer, updates its clipboard, and pastes into the
  shared window.

Only nonempty plain UTF-8 text up to 1 MiB is transferred. Files, file lists,
file promises, images, and other rich clipboard formats are not transferred.
Text that happens to look like a path is still ordinary text.

These keyboard shortcuts are **not** a supported way to copy and paste only
within the sharer's computer. A Copy followed by a Paste can pass eligible
plain text through your clipboard, but it can lose rich formatting or other
clipboard data, so Petal does not treat it as native or lossless behavior. If
you need B-local Copy/Paste, use the shared application's own context menu,
toolbar, or dropdown for **both** operations, when that UI is reachable inside
the shared window. Do not use an application-menu Copy followed by Petal's
keyboard Paste: Petal's keyboard Paste intentionally replaces the sharer's
clipboard with your clipboard first. A global menu bar or other UI outside the
shared window may not be reachable; without a suitable in-window UI, B-local
Copy/Paste is unsupported.

The clipboard feature is for native desktop-to-desktop control only. Browser
participants do not implement these native clipboard streams.

## Local echo (experimental)

If you're the one controlling someone else's window, **Settings → Privacy & Sharing → "Local echo (experimental)"** controls whether you see instant feedback for your own input — a small ripple for clicks, a pending-text indicator while typing — before the real updated frame comes back from the sharer's machine. It's off by default. When it's on, what you see is a prediction of what you just sent, not confirmation that it actually happened on the sharer's window — the real frame is still the source of truth. This setting only affects your own view while you're controlling; it has no effect on what the person sharing sees.
