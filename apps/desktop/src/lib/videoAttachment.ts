export function attachVideoStream(
  videoEl: HTMLVideoElement | null,
  videoStream: MediaStream | null | undefined
): boolean {
  if (!videoEl) return false;

  const nextStream = videoStream ?? null;
  if (videoEl.srcObject === nextStream) return false;

  videoEl.srcObject = nextStream;
  return true;
}
