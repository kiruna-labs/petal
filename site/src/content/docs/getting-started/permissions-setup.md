---
title: Permissions setup
description: Granting Petal the macOS permissions it needs to share your screen.
---

This page is macOS-only: Windows has no privacy-permission model, and the
Windows build skips the permission checklist.

Petal needs a handful of macOS privacy permissions to work. It asks for them
one at a time the first time you launch the app, and you can revisit all of
them later from **Settings → Permissions**.

Three of the four — Screen Recording, Microphone, and Accessibility — are
required: setup won't let you continue into the app until they're granted,
and if one is later revoked, Petal takes you back through setup on the next
launch. Camera is optional and can be skipped.

## The four permissions

### Screen Recording

Petal can only share the windows you choose. Nothing else is visible unless
you explicitly share it — but macOS still requires Screen Recording access
before Petal can capture any window content at all.

This is required to complete setup.

### Microphone

Lets teammates hear you when you join a meeting.

This is required to complete setup.

### Camera

Lets teammates see you on camera when you want. This is the one optional
permission: you can skip it during setup, always join meetings with your
camera off, and grant it later from **Settings → Permissions** if you change
your mind.

### Accessibility

Petal uses this to replay approved remote-control clicks and keystrokes into
a window you're sharing — so a teammate can drive your shared window when you
let them. Without it, remote control into your shared windows won't work.

This is required to complete setup.

## Granting a permission

Petal asks for the permissions one at a time, in order. Click the current
row's action button — **Set up Screen Recording**, **Allow Microphone**,
**Allow Camera**, **Allow Accessibility** — and macOS shows its own system
prompt; approve it there.

macOS only picks up a fresh Screen Recording or Accessibility grant after
the app restarts, so when you grant one of these during setup, Petal
restarts itself automatically (you'll briefly see a "Relaunching Petal"
notice). If the automatic restart doesn't go through, a **Relaunch
required** notice appears with a **Relaunch now** button.

## If you denied a permission by accident

During setup, a denied permission row shows an **Open System Settings**
button — grant the permission there, then come back. A **Recheck
permissions** button appears below the checklist once Screen Recording is
granted and another row is still denied; the Accessibility row has its own
**Recheck Accessibility** button, and if Accessibility still looks off after
you enabled Petal, the row walks you through removing the stale Petal entry
from the Accessibility list and restarting.

After setup, open **Settings** inside Petal and go to the **Permissions**
section. Any permission that's currently off shows as needing attention,
with the same **Open System Settings** button. Clicking it takes you
straight to the correct System Settings privacy pane for that permission
(Screen Recording, Microphone, Camera, or Accessibility) instead of the
general Privacy & Security list. For Screen Recording, macOS may offer to
quit and reopen Petal when you flip the toggle — accept that, or relaunch
Petal yourself, so the new grant takes effect.
