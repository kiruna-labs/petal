---
title: Viewing shared windows
description: How shared windows appear and behave on your machine.
---

When a teammate shares a window, it opens on your desktop as a real, independent window — not a tile in a grid, and not embedded inside Petal's own window. You can move it, resize it, and position it wherever you want, and it keeps updating live wherever you put it.

## Where it appears

The first shared window you receive opens near the top-left of your primary display. If more windows are shared (by the same person or others), each new one is offset slightly down and to the right so they don't stack exactly on top of each other. Petal doesn't remember window positions between meetings — every new share starts from this default placement.

## The header bar

Every shared window has a header bar docked to its top edge. It shows:

- **Window controls** on the left — three macOS-style dots. The close dot is disabled; the second dot **hides** the window, and the third dot **fits to source size** (resizes the window to match the sharer's actual window dimensions).
- **The window's title** — the source window's name and who's sharing it, shown as "*source* by *owner*".
- **A status chip**, when relevant — for example, "Video paused" or a remote-control status.
- **Debug** — toggles a per-window diagnostics panel with frame and latency stats.
- **Open URL** — appears only when the shared window has a page address to open (for example, a shared browser tab).
- **View / Control / Draw mode switcher** on the right — lets you switch between just watching, requesting keyboard/mouse control, and drawing on the window. See [Remote control](/docs/using/remote-control/) for what Control does.

The header is always visible — it never auto-hides — and it doubles as the window's drag handle: drag anywhere on it to move the window.

Hiding a window only closes it on your machine; the share keeps running for everyone else. A hidden window doesn't come back on its own — it reopens if the sharer stops and re-shares that window, or when you leave and rejoin the meeting.

## Resizing

Shared windows always keep the source window's aspect ratio. To resize one, drag the grips on the header strip: the thin zone along the top edge, the header's left or right edge, or a top corner. (There are no resize grips along the bottom — the area below the header belongs entirely to the video.) Any size works; if you release the drag close to exactly 1x or 2x of the sharer's real window size, Petal snaps to that exact multiple so text and UI elements stay pixel-sharp. To get back to the sharer's real size, use the fit-to-source button in the header.

## When a share ends

If the person sharing stops sharing that window, or leaves the meeting, the window closes on your machine automatically.
