// Native camera → webview self-view feed, JS consumer.
//
// Windows self-view used to be an SFU round trip: the native Media Foundation
// publication was re-subscribed by the hidden gallery-bridge participant and
// rendered in a `<video>` tile (encode → SFU relay → decode latency, plus a
// freeze watchdog). Now the native camera frames are pulled directly from the
// Rust process (`next_self_view_frame` → raw ArrayBuffer; layout documented in
// src-tauri/src/camera_self_view.rs) and drawn onto an offscreen canvas whose
// `captureStream(60)` output feeds the same local tile as before — no network
// round trip, and the camera light is on exactly once (one capture client).
//
// The captureStream rate is a CAP, not a target: the canvas only paints when a
// genuinely new frame arrives, so the stream never fabricates frames beyond
// what the camera actually delivers. It was hardcoded to 30 — with the 60fps
// setting selected the self-preview could never exceed 30fps even though the
// Media Foundation capture negotiates the 60fps mode. The pull loop also used
// to await the IPC round trip inside the rAF tick, throttling the pull rate
// below the display rate; the invoke now overlaps the next vsync instead.
//
// WebCodecs `VideoFrame` + `canvas.captureStream` are Chromium APIs —
// available in evergreen WebView2 (Chromium ≥ 94); no fallback needed.

import { invoke } from '@tauri-apps/api/core';
import { COMMANDS } from '$lib/ipc';

/** `[width: u32][height: u32][capture_wall_time_us: u64]` little-endian. */
const SELF_VIEW_HEADER_BYTES = 16;

/** Self-view stream cap — the highest fps preset the UI can request. The
 * canvas only paints when a new frame arrives, so this is a ceiling, never a
 * frame factory: a 30fps capture yields a 30fps stream under this cap. */
const SELF_VIEW_MAX_FPS = 60;

let stream: MediaStream | null = null;
let rafId: number | null = null;
let cancelled = false;
let pulling = false;

/** Start the native-fed self-view stream. Idempotent: a prior feed (if any)
 * is stopped first, so start/stop can never stack two pull loops. */
export async function startNativeSelfView(): Promise<MediaStream> {
  stopNativeSelfView();
  cancelled = false;

  const canvas = document.createElement('canvas');
  canvas.width = 640;
  canvas.height = 360;
  const ctx = canvas.getContext('2d');
  if (!ctx) {
    stopNativeSelfView();
    throw new Error('2d canvas context unavailable');
  }
  stream = canvas.captureStream(SELF_VIEW_MAX_FPS);
  let lastDrawnTimestamp = -1;

  // Pull cadence: at most one invoke in flight, started from a FRESH rAF tick
  // every time — the IPC round trip overlaps the next vsync instead of being
  // awaited inside the tick (awaiting capped the pull rate below the display
  // rate). rAF ticks share the display period with a 60fps capture (and
  // outpace slower ones), so each tick catches the newest frame; drawing only
  // genuinely new frames keeps the canvas paint rate at the camera's real
  // cadence. VideoFrame layout offsets skip the 16-byte header directly, so
  // the per-draw `buf.slice()` full-frame copy is gone too.
  const loop = () => {
    if (cancelled) return;
    if (!pulling) {
      pulling = true;
      invoke<ArrayBuffer>(COMMANDS.nextSelfViewFrame)
        .then((buf) => {
          if (buf.byteLength === 0) return;
          const view = new DataView(buf);
          const width = view.getUint32(0, true);
          const height = view.getUint32(4, true);
          const timestamp = Number(view.getBigUint64(8, true));
          if (timestamp === lastDrawnTimestamp) return;
          lastDrawnTimestamp = timestamp;
          const frame = new VideoFrame(buf, {
            format: 'NV12',
            codedWidth: width,
            codedHeight: height,
            layout: [
              { offset: SELF_VIEW_HEADER_BYTES, stride: width },
              { offset: SELF_VIEW_HEADER_BYTES + width * height, stride: width }
            ],
            timestamp
          });
          if (canvas.width !== width || canvas.height !== height) {
            canvas.width = width;
            canvas.height = height;
          }
          ctx.drawImage(frame, 0, 0);
          frame.close();
        })
        .catch((error) => {
          console.error('native self-view: frame pull failed', error);
        })
        .finally(() => {
          pulling = false;
        });
    }
    rafId = requestAnimationFrame(loop);
  };
  rafId = requestAnimationFrame(loop);
  return stream;
}

/** Stop the native-fed self-view: cancel the pull loop and stop the canvas
 * stream's tracks. Idempotent. */
export function stopNativeSelfView(): void {
  cancelled = true;
  if (rafId !== null) cancelAnimationFrame(rafId);
  rafId = null;
  stream?.getTracks().forEach((track) => track.stop());
  stream = null;
}
