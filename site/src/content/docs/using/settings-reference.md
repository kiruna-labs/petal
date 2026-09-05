---
title: Settings reference
description: What every setting in Petal does.
---

Open Settings from the profile menu in the main window (click your avatar,
then **Settings**) or from the menubar popover's **Settings** item. This
page covers every section, top to bottom.

## Devices

- **Camera preview** — a live feed from your selected camera, shown once
  Camera permission is granted. If access is denied, this box instead shows
  a button that opens System Settings to the Camera privacy pane.
- **Camera** — picks which camera feeds the preview above. Camera options
  are populated from your Mac's real cameras once permission is granted;
  before that, a single placeholder option is shown. Note: this only
  affects the preview — the camera published in a meeting is always the
  system default camera, and the selection isn't saved (see
  [Cameras and audio](/docs/using/cameras-and-audio/)).
- **Microphone** — picks your recording device. Applies immediately if
  you're in a meeting; otherwise saved for your next join.
- **Speaker** — picks your playback device. Same immediate-vs-saved behavior
  as microphone.

## Permissions

Four rows, each showing whether Petal currently has that permission and a
button to open the matching System Settings privacy pane if it doesn't:

- **Screen Recording** — required to share your screen or windows.
- **Microphone** — required to speak in a meeting.
- **Camera** — needed to appear on camera and for the Settings preview.
  Unlike the other three, it isn't needed to get through onboarding — you
  can always join meetings with your camera off and grant it later.
- **Accessibility** — required for remote control: it's what lets Petal
  replay a teammate's clicks and keystrokes into windows you share while
  remote control is on.

Use this section any time you accidentally denied a permission during
onboarding — it's the same recoverable flow, just reachable later.

## Privacy & Sharing

- **Allow teammates to control my shared windows** — on by default. When on,
  people in your meetings can remote-control windows you share. Turning it
  off takes effect immediately — anyone currently controlling one of your
  windows loses control on the spot — and it stays your default for future
  meetings. The in-meeting controls can also stop control for just the
  current call without changing this default.
- **Local echo (experimental)** — off by default. When you're the one
  controlling someone else's shared window, this shows an instant local
  preview of your own clicks, keystrokes, and typing (a ripple, a pending
  text strip) before the real frame comes back over the network. It's a
  prediction, not confirmation that the action actually landed remotely.

## Diagnostics

- **Export logs** — reveals a zip of your local Petal logs in your file
  manager (Finder on macOS, Explorer on Windows) so you can attach it to a
  bug report. Nothing leaves your Mac on its own.
- **Send crash and error reports to Sentry** — on by default. Sends
  diagnostic data (not just crashes) to help identify problems. Turn off if
  you don't want any diagnostic data leaving your Mac.

## Reset

- **Reset Petal** — clears your local identity, saved rooms, favorites,
  microphone/speaker choices, and saved window positions on this Mac, then
  quits the app. This is local-app state only; see
  [Troubleshooting](/docs/using/troubleshooting/#factory-reset) for exactly what
  it does and doesn't touch, including macOS permissions.

## Updates

- **Check for updates** — Petal checks for updates automatically on launch;
  this button forces an immediate check instead of waiting. The current
  version and commit are shown next to it.

## Account

- **Display name** — the name shown on your tile and in the participant
  list.
- **Identity color** — one of six accent colors used for your cursor,
  pointer, and name chip elsewhere in the app, so teammates can tell whose
  pointer or annotation is whose.
