export function cameraPreviewConstraints(deviceId?: string): MediaTrackConstraints {
  const constraints: MediaTrackConstraints = {
    width: { ideal: 1280 },
    height: { ideal: 720 },
    frameRate: { ideal: 30 }
  };
  if (deviceId) constraints.deviceId = { exact: deviceId };
  return constraints;
}
