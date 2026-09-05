export const HARNESS_NAME_STORAGE_KEY = 'petal-harness-name';
export const HARNESS_COLOR_STORAGE_KEY = 'petal-harness-user-color';
export const HARNESS_ROOM_STORAGE_KEY = 'petal-harness-last-room';
export const HARNESS_RECENTS_STORAGE_KEY = 'petal-harness-recent-rooms';
export const HARNESS_FAVORITES_STORAGE_KEY = 'petal-harness-favorite-rooms';
export const HARNESS_ROOM_DISPLAY_NAMES_STORAGE_KEY = 'petal-harness-room-display-names';
export const HARNESS_TILE_LAYOUT_STORAGE_KEY = 'petal-harness-tile-layout';
export const HARNESS_AUDIO_INPUT_STORAGE_KEY = 'petal-harness-audio-input';
export const HARNESS_AUDIO_OUTPUT_STORAGE_KEY = 'petal-harness-audio-output';
export const HARNESS_VIDEO_INPUT_STORAGE_KEY = 'petal-harness-video-input';
// Refs #378: opt-in, default OFF (mirrors desktop's session.localEchoEnabled
// -- see apps/desktop/src/lib/stores/session.svelte.ts).
export const HARNESS_LOCAL_ECHO_STORAGE_KEY = 'petal-harness-local-echo-enabled';
// #669: opt-in, default OFF -- gates the remote-window header's Debug
// button (mirrors desktop's Rust-owned debug_settings.rs, minus the
// cross-webview propagation problem localStorage has no way to solve there).
export const HARNESS_DEBUG_MODE_STORAGE_KEY = 'petal-harness-debug-mode-enabled';
export const MAX_RECENT_ROOMS = 8;

export const CAMERA_VIDEO_CONSTRAINTS: MediaTrackConstraints = {
  width: { ideal: 1280 },
  height: { ideal: 720 },
  frameRate: { ideal: 30, max: 30 },
};

export const CAMERA_VIDEO_ENCODING = {
  maxBitrate: 2_500_000,
  maxFramerate: 30,
} as const;

// Explicit encoding for the cockpit test-pattern window share. Without this,
// livekit-client applies its DEFAULT ScreenShare preset, whose maxFramerate is
// 15fps (tuned for slides) -- which capped SHARE-W2N-Q at a steady 15fps and
// failed the fps>20 gate even though Petal's receive path was healthy. The
// pattern is real 30fps motion (Gray-code counter + moving object), so publish
// it at 60fps with headroom bitrate for 960x600. (#254/#383 fps ceiling.)
export const TEST_PATTERN_SCREENSHARE_ENCODING = {
  maxBitrate: 3_000_000,
  maxFramerate: 60,
} as const;

/// Frames per second for a REAL screen share. 30, not livekit's default 15:
/// 15fps reads as visibly choppy, and 30 is adequate for this product's
/// content (CLAUDE.md: frame rate is not the quality lever here).
export const SCREENSHARE_MAX_FRAMERATE = 30;

/**
 * Bitrate ceiling for a REAL screen share, scaled by the captured pixel count.
 *
 * These buckets deliberately mirror the NATIVE publisher's macOS ladder in
 * `apps/desktop/src-tauri/src/transport/publisher.rs` (`video_encoding`), so a
 * browser sharer and a native sharer of the same display look alike. Keep the
 * two in step if either changes.
 *
 * Why this exists at all: without an explicit encoding, livekit-client applies
 * `ScreenSharePresets.h1080fps15` — 1920x1080, **2.5 Mbps, 15fps**. Split
 * across simulcast layers that starves the top layer, and the encoder resolves
 * the shortfall by shedding resolution. Measured in the field on 2026-09-01: a
 * 2560x1600 desktop arrived at the native receiver as a 640x360 source
 * publishing 320x180 frames at exactly 15.0fps — text was unreadable for the
 * whole 37-minute call. The native path was never affected because it has
 * always set this ceiling itself (4–18 Mbps).
 */
/**
 * Publish options for a REAL screen share, as a pure function of the captured
 * dimensions.
 *
 * Extracted so it is TESTABLE. `getDisplayMedia` opens a browser picker that
 * "cannot be clicked by unattended automation" (controls.ts), so every cockpit
 * scenario drives the synthetic test-pattern share instead -- meaning no
 * end-to-end scenario has ever exercised this path. The 2026-09-01 field bug
 * lived precisely in that blind spot: the real share fell through to
 * livekit's default preset while the test-pattern share it was assumed to
 * match set an explicit one. The contract tests over this function are the
 * only thing standing in for a scenario that cannot exist.
 */
export function screenSharePublishEncoding(
  capturedWidth: number | undefined,
  capturedHeight: number | undefined
): { maxBitrate: number; maxFramerate: number } {
  return {
    maxBitrate: screenShareMaxBitrate(capturedWidth ?? 0, capturedHeight ?? 0),
    maxFramerate: SCREENSHARE_MAX_FRAMERATE,
  };
}

export function screenShareMaxBitrate(width: number, height: number): number {
  const pixels = Math.max(0, Math.round(width)) * Math.max(0, Math.round(height));
  if (pixels <= 0) return 8_000_000; // dimensions unknown yet — assume a typical laptop display
  if (pixels <= 1_500_000) return 4_000_000;
  if (pixels <= 3_000_000) return 8_000_000;
  if (pixels <= 6_000_000) return 12_000_000;
  return 18_000_000;
}
