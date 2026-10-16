// Which room's meeting route is mounted (and so owns a lost connection's
// rejoin + toasts, #354). Plain module state: ToastHost and the meeting
// route live in the same main webview.
let mountedRoom: string | null = null;

export function setMeetingRouteRoom(room: string): void {
  mountedRoom = room;
}

export function clearMeetingRouteRoom(room: string): void {
  if (mountedRoom === room) mountedRoom = null;
}

/** A lost connection nobody will rejoin must still be visible. */
export function connectionLossNeedsToast(event: { roomName: string; connectionLost?: boolean }): boolean {
  return !!event.connectionLost && mountedRoom !== event.roomName;
}
