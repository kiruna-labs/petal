---
title: Sharing your windows
description: How window sharing works in Petal and how to start one.
---

Petal shares individual windows, not your whole screen. You can share more than one window at a time, and each one is a separate share your teammates can move and resize independently on their own desktops.

## Start sharing with the hover tab

While you're in a meeting, move your cursor over any eligible window you want to share. Petal shows one fixed 40×40 square on a vertical rail at the window's right edge. It sits just outside the window when the monitor work area has room and insets at the right edge when necessary, so its position is safe for ordinary, maximized, and top-aligned windows on either platform.

Click the square—or press Enter or Space when it has focus—to perform the direct action. An unshared window starts sharing; a shared window stops sharing. The button is disabled while the request is pending. Its tooltip says that you can drag to move it and right-click for options.

### Move the hover tab

The square is also the drag surface. Motion below 6px remains a normal Share/Stop click. Once movement reaches 6px, drag vertically to move the square along the source window's right-edge rail; the button follows in real time and the eventual click is suppressed. Release to commit the position. Petal stores one normalized position for the app (`0` top, `0.5` center, `1` bottom), so it applies to later windows and survives restart.

Press **Escape**, cancel the pointer gesture, or allow pointer capture to be lost to cancel instead of committing. A missing or malformed saved position safely returns to right-center. The native surface remains exactly 40×40 and never becomes a free-floating content-covering control.

Right-click the square to open Petal's existing system-native sharing-options menu. When the square has focus, **Shift+F10** or the keyboard **Menu** key opens the same menu beside it. The hover menu contains sharing priority, **Hover tab position** (Top, Center, Bottom), Windows control mode, Draw, AI Chat, and Debug options. Opening the menu never starts or stops sharing, and the tab remains 40×40 while it is open. The position choices are the keyboard-accessible alternative to dragging.

Once a window is shared, Petal draws a rounded border around it on your screen in your identity color, so you can tell at a glance which of your windows are live. The square changes to the same identity color and shows a live marker. On Windows, Petal asks once whether it may replace the system capture outline with this indicator. If Windows denies that request or the replacement cannot be made safe, sharing still starts with Windows' native yellow capture indicator instead. Teammates don't see your local indicator — on their machines, the shared window itself appears as its own window, with your name and color on its header.

## Stop sharing

For an ordinary shared window, click its right-edge square once. The action is always **Stop sharing** while it is live, including while Draw is active. To stop Draw without stopping the share, right-click the square and choose **Stop drawing on this window** from the native menu. Draw keeps the fixed tab reachable after the pointer leaves the window. Petal View uses its own persistent title-bar Share/Stop button instead.

## Petal View

When you choose a region with **Petal View**, the selector has persistent **Options**, Share/Stop, and Close controls in its title bar. Use **Options** for sharing priority, Draw, AI Chat, and Debug; it deliberately has no hover-tab position entries. The right-edge hover square is blocked over the selector and through its hollow interior. The selector stays high-contrast while idle and switches to your identity color while shared. On Windows, an idle Petal View is visible to supported screen recorders such as OBS, so you can record a demo of its controls. When that selector is actively shared, Windows intentionally excludes its frame and controls from capture to prevent the selector from appearing in its own shared video; it becomes recordable again after sharing stops.

## Sharing from the window picker

You can also click **Share a window** in the meeting controls to open the window picker: a list of your open windows with thumbnail previews, where you can start or stop sharing without hunting for the window on screen. It uses the same share/unshare action as the hover tab, so the two stay in sync.

## Sharing priority

The hover tab's native options menu opens from right-click, **Shift+F10**, or the keyboard **Menu** key. It contains four sharing priorities: **Automatic (recommended)**, **Responsive: smoother control**, **Sharp text: preserve detail**, and **Data saver: 15 fps, slower control**. Petal View exposes the same priority menu from its title-bar **Options** button. Picking one takes effect immediately for an active share when supported, and also becomes the default for windows you share afterwards. Priority and hover-tab position are stored together in the native sharing-preferences file; pointer previews do not write the file until you commit.

## Drawing on a shared window

If you want to point something out visually on a window you're sharing, open the native options menu and choose **Draw on this shared window** (the entry is only enabled while the window is actually shared, and reads **Stop drawing on this window** while drawing is active). The hover tab remains fixed; its primary click still means **Stop sharing**. Everyone viewing that window sees your strokes live.

## Full-control requests on Windows

Windows starts ordinary window shares in cursor-preserving mode. If a controller requests full control, the request appears in Petal's existing non-activating consent panel with the participant and window name. **Allow** changes that share to full control; **Deny** leaves it cursor-preserving. The request expires after 30 seconds and always fails closed. It never resizes or adds content to the hover tab.
