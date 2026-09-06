---
title: Troubleshooting
description: Fixes for common Petal problems.
---

## A permission was denied

If you accidentally denied Screen Recording, Microphone, Camera, or
Accessibility during setup, you don't need to reinstall anything:

1. Open **Settings > Permissions**.
2. Find the denied row and click its **Open System Settings** button — this
   deep-links straight to the correct macOS privacy pane (Screen Recording,
   Microphone, Camera, or Accessibility) instead of the generic Privacy &
   Security page.
3. Enable Petal in that list.
4. Come back to Petal. For Camera specifically, the Settings preview has a
   **Try again** button that rechecks the permission without relaunching.
   For other permissions, relaunch Petal if the change doesn't take effect
   immediately.

If Camera permission shows as granted in System Settings but the preview in
Settings still stays dark, relaunch Petal — this is a known WebKit
attribution quirk where the app-level permission check and the actual camera
request can briefly disagree after a fresh grant.

## Camera or microphone is in use by another app

If another app (Zoom, FaceTime, a browser tab, and so on) is holding your
camera, Petal's camera preview reports "camera is in use by another app"
instead of showing video. Quit or stop the other app's use of the camera,
then re-select your camera in the **Camera** dropdown (or close and reopen
Settings) to retry the preview. The **Try again** button only appears in
the permission-denied state, not here. The same applies if your microphone
track fails to publish — check nothing else has it locked.

## A device disappeared from the list

If a microphone or speaker you previously selected gets unplugged, Petal
falls back to the first available device in that dropdown rather than
leaving the selection pointing at a device that no longer exists. Reconnect
the device and reselect it in **Settings > Devices** if you want it back.

## An update seems stuck

Petal checks for updates automatically on launch. If you want to check
right now instead of waiting:

1. Open **Settings > About**.
2. Click **Check for updates**.
3. The result appears inline: you're up to date, an update is ready (look
   for the "Restart now" toast to install it), an update installed and
   Petal is relaunching, or an error with a specific reason.

If it reports an error, note the message — it's the real underlying failure
string, not a generic "something went wrong."

## Exporting logs for a bug report

1. Open **Settings > Diagnostics**.
2. Pick a range — **Last 2 days**, **Last 7 days**, or **All logs** — and
   click **Export logs**.
3. Petal reveals a zip of your local logs in Finder (Explorer on Windows), or
   reports the saved path if it can't. Nothing is uploaded automatically —
   attach the zip yourself to whatever bug report or email you're sending.

Logs also live directly in `~/Library/Logs/Petal/` (macOS) or
`%APPDATA%\Petal\logs\` (Windows) if you need to tail them live while
reproducing a problem. The active file is `petal.log.<YYYY-MM-DD>` (UTC
dates; it rolls at midnight, and older days are gzipped).

## Factory reset

Reset Petal is the last resort — use it if the app is in a broken state that
permission fixes and updates don't resolve.

**What it clears**, from **Settings > About > Reset…**:

- Your local identity (display name, identity color) and onboarding state
- Saved rooms and favorites
- Device choices (microphone/speaker/camera selections)
- Saved window positions and sizes (main window, meeting window, pill
  window, window-picker layout)

Petal quits automatically after resetting.

**What it leaves alone:** your sharing preferences (sharing priority and
hover-tab position), the **Debug mode** setting, and your AI chat settings —
including a saved Gemini API key. Remove the key from **Settings > AI chat**
yourself if you're handing the machine to someone else.

**What it does not clear on its own (macOS only):** macOS permissions
(Screen Recording, Microphone, Camera, Accessibility). If you also want to reset those — for
example, to force the OS permission prompts to reappear from scratch —
Petal shows the exact Terminal commands to run after it quits:

```
tccutil reset ScreenCapture com.petal.app
tccutil reset Accessibility com.petal.app
tccutil reset Microphone com.petal.app
tccutil reset Camera com.petal.app
```

Petal also copies these commands to your clipboard when you confirm the
reset (when clipboard access is available — if the copy fails, it says so
and the commands stay visible above the confirm button). Run them in
Terminal, then relaunch Petal to go through permission setup again.
