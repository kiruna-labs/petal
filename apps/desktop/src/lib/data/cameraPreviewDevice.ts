// The camera picker stores a native (AVFoundation) device id, but the WebKit
// previews open the camera through getUserMedia, whose deviceIds are salted
// per origin and share nothing with the native ids. Without a bridge the
// previews asked for no device at all and showed the system default, so with
// more than one camera attached the self-view and the published track could
// be different cameras (#312).
//
// Both sides report the same human-readable device name, so the bridge is the
// name: find the selected native camera's name, then the one WebKit camera
// whose label matches it exactly. Anything ambiguous (no selection, unknown
// id, unlabeled devices, two cameras with the same name) resolves to
// undefined, which keeps the old default-camera behavior.

export interface NativeCamera {
  id: string;
  name: string;
}

export interface WebCamera {
  deviceId: string;
  label: string;
}

export function previewCameraIdFor(
  nativeId: string | null | undefined,
  nativeCameras: readonly NativeCamera[],
  webCameras: readonly WebCamera[]
): string | undefined {
  if (!nativeId) return undefined;
  const name = nativeCameras.find((c) => c.id === nativeId)?.name.trim();
  if (!name) return undefined;
  const matches = webCameras.filter((c) => c.deviceId && c.label.trim() === name);
  return matches.length === 1 ? matches[0].deviceId : undefined;
}

export function webCamerasFrom(devices: readonly MediaDeviceInfo[]): WebCamera[] {
  return devices
    .filter((d) => d.kind === 'videoinput')
    .map((d) => ({ deviceId: d.deviceId, label: d.label }));
}

/** The deviceId of the camera a stream is actually reading from, if WebKit reports it. */
export function streamCameraId(stream: MediaStream): string | undefined {
  return stream.getVideoTracks()[0]?.getSettings().deviceId || undefined;
}
