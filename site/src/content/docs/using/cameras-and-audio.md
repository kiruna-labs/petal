---
title: Cameras and audio
description: Using your webcam and microphone in a Petal meeting.
---

## Camera tiles

When your camera is on, you appear as a video tile in the meeting gallery
(your own tile is mirrored, like a mirror image of yourself). When your
camera is off, your tile shows your display name centered on a flat graphite
background instead of video, falling back to just your first letter if the
full name doesn't fit. Camera video always renders as a regular gallery tile
— it never becomes a movable window. Movable native windows are reserved for
shared windows and screens, not cameras.

A muted participant's tile shows a small mic-off icon in the corner. If a
participant's connection is weak, their tile dims and shows a "Video paused"
indicator until the connection recovers.

## Switching devices mid-meeting

Open **Settings > Devices** to change your camera, microphone, or speaker.

- **Microphone and speaker** changes apply immediately if you're currently in
  a meeting — the dropdown reports "Switched microphone" or "Switched
  speaker" once the swap succeeds. If you're not in a meeting, the choice is
  saved for later and the dropdown reports "Saved — applies when you join a
  room."
- **Camera** selection in Settings only drives the live preview shown right
  there in Settings, using whichever cameras macOS reports once Camera
  permission is granted. The camera actually published in a meeting is
  always the system default camera (usually the built-in one) — picking a
  different camera in Settings does not change which camera other
  participants see, and the choice isn't saved between visits to Settings.
  This is a real limitation of the current version, not a hidden setting
  you're missing.

If your microphone shows "isn't active in this meeting" after a switch,
check that Microphone permission is granted under **Settings >
Permissions** — a device swap can't hot-swap a track that was never
publishing in the first place.

## No devices found

If Petal can't find any microphones or speakers, the corresponding dropdown
in Settings is disabled and reads "No microphones found" or "No speakers
found." This reflects what macOS itself reports. Reconnect or check your
audio hardware, then reopen Settings.

## Camera permission is optional

You don't need to grant Camera permission to use Petal — you can always
join meetings with your camera off. (Onboarding requires Screen Recording,
Microphone, and Accessibility; Camera can be granted whenever you first
want to appear on camera.)

If Camera permission is denied, the preview box in **Settings > Devices**
shows "Camera access is turned off for Petal" along with a button that
opens System Settings directly to the Camera privacy pane, and a "Try
again" button to recheck once you've granted it. See
[Troubleshooting](/docs/using/troubleshooting/) for the full recovery flow,
including what to do if the permission looks granted but the preview stays
dark.
