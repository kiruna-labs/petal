---
title: Settings reference
description: What every setting in Petal does.
---

Open Settings from the profile menu in the main window (click your avatar,
then **Settings**) or from the menubar popover's **Settings** item. A row of
chips under the title jumps to each section; the sections appear in this
order, top to bottom: Devices, Permissions (macOS only), Privacy & Sharing,
AI chat, Diagnostics, Account, About. Every on/off setting is a switch on its
row and takes effect as soon as you flip it — except AI chat, which asks you
to confirm first.

## Devices

- **Camera preview** — a live feed from your selected camera, shown once
  Camera permission is granted. If access is denied, this box instead shows
  **Open System Settings** (straight to the Camera privacy pane) and **Try
  again**.
- **Camera** — picks the camera Petal publishes. If you're in a meeting the
  switch applies immediately ("Switched camera"); otherwise it's saved for
  your next join. The choice is remembered across launches. If the saved
  camera isn't connected, Petal falls back to the default one and says so.
- **Camera resolution** and **Camera frame rate** (Windows only) — pick a
  capture mode; modes the camera doesn't support are greyed out.
- **Microphone** — picks your recording device. Applies immediately if
  you're in a meeting; otherwise saved for your next join.
- **Speaker** — picks your playback device. Same immediate-vs-saved behavior
  as microphone.

## Permissions (macOS)

Four rows, each showing whether Petal currently has that permission and an
**Open System Settings** button that opens the matching privacy pane if it
doesn't:

- **Screen Recording** — required to share your screen or windows.
- **Microphone** — required to speak in a meeting.
- **Camera** — needed to appear on camera and for the Settings preview. It's
  the one permission setup lets you skip; grant it here whenever you first
  want to appear on camera.
- **Accessibility** — required for remote control: it's what lets Petal
  replay a teammate's approved clicks and keystrokes into windows you share.

Use this section any time you denied a permission during setup — it's the
same recoverable flow, just reachable later. Windows has no permission model,
so this section doesn't appear there.

## Privacy & Sharing

- **Remote control of my shared windows** — one of three:
  **Ask me each time** (default: "You approve or deny every request."),
  **Allow automatically** ("Anyone in the meeting can take control."), or
  **Off** ("Requests are refused."). Changing it takes effect immediately —
  anyone currently controlling one of your windows loses control on the
  spot — and it stays your default for future meetings. The meeting bar's
  **More** menu can turn it off for just the current call. See
  [Remote control](/docs/using/remote-control/).
- **Local echo (experimental)** — off by default. When you're the one
  controlling someone else's shared window, this shows an instant local
  preview of your own clicks and typing (a ripple, a pending text strip)
  before the real frame comes back. It's a prediction, not confirmation.
- **Debug mode** — off by default. Adds a **Debug** button to every remote
  window's header, showing frame counters, glass-to-glass latency, and packet
  loss for that share.

## AI chat

- **AI chat on shared windows** — off by default. Flipping the switch on
  opens a confirmation step that spells out both consequences before
  anything changes: you get an AI chat button on every shared window in your
  meetings (the ones you share and the ones other people share), and anyone
  in your meetings can start AI chat on a window you share, which sends that
  window's content and the room's voice to Google while a session is live.
  **Turn on AI chat** confirms; **Cancel** leaves it off. Turning it off
  again is immediate. See [AI chat](/docs/using/ai-chat/).
- **Gemini API key (optional)** — appears once AI chat is on (or a key is
  already saved). Bring your own key so AI chat bills your own Google account
  (roughly 2–4¢ per minute). Free-tier keys may allow Google to use content
  to improve their models. **Save** stores it on this machine; **Remove**
  deletes it.

## Diagnostics

- **Export logs** — reveals a zip of your local Petal logs in Finder (or
  Explorer on Windows) so you can attach it to a bug report. The dropdown
  beside it picks the range: **Last 2 days**, **Last 7 days**, or **All
  logs**. Nothing leaves your machine on its own.
- **Send crash and error reports to Sentry** (macOS) — on by default. Sends
  diagnostic reports (not just crashes) to help identify problems. Turn it
  off if you don't want any diagnostic data leaving your Mac.

Petal also records a small set of anonymous product events (things like
"meeting joined", "share started", "reconnect recovered") to help the
maintainers see where real users hit trouble. The exact event list is
published in the repository's `docs/POSTHOG_EVENT_ALLOWLIST.md`, and it
never includes window titles, room names, or content. There is no in-app
toggle for it yet; a build from source without an analytics key sends
nothing.

## Account

- **Display name** — the name shown on your tile, in the participant list,
  and on the header of every window you share.
- **Identity color** — one of six accent colors (plum, blue, green, amber,
  lilac, slate) used for your cursor, your share border, and your name chip,
  so teammates can tell whose pointer or annotation is whose.

## About

The section header shows the version and build you're running.

- **Updates → Check for updates** — Petal checks automatically on launch;
  this button forces an immediate check and reports the result inline.
- **Reset Petal → Reset…** — after a confirmation ("Reset and quit"), clears
  your local identity, saved rooms and favorites, microphone/speaker/camera
  choices, and saved window positions on this machine, then quits the app.
  It does not touch macOS permissions; see
  [Troubleshooting](/docs/using/troubleshooting/#factory-reset) for exactly
  what it does and doesn't clear.
