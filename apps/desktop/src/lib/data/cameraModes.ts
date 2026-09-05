import { invoke } from '@tauri-apps/api/core';
import { session } from '$lib/stores/session.svelte';
import { COMMANDS, hasTauriBridge } from '$lib/ipc';
import type { AppliedCameraDevice, CameraMode } from '$lib/ipc';

export { hasTauriBridge } from '$lib/ipc';
export type { AppliedCameraDevice, CameraMode } from '$lib/ipc';

/** The resolution presets offered in Settings (16:9 / 4:3 fixed sizes). */
export interface CameraResolutionPreset {
  id: string;
  width: number;
  height: number;
  label: string;
}

export const CAMERA_RESOLUTION_PRESETS: CameraResolutionPreset[] = [
  { id: 'auto', width: 0, height: 0, label: 'Auto (best)' },
  { id: '480p', width: 640, height: 480, label: '480p' },
  { id: '720p', width: 1280, height: 720, label: '720p' },
  { id: '1080p', width: 1920, height: 1080, label: '1080p' },
  { id: '4k', width: 3840, height: 2160, label: '4K (2160p)' }
];

/** The FPS presets offered in Settings. */
export const CAMERA_FPS_PRESETS: number[] = [15, 30, 60];

/** 29.97-style rates round to the nearest integer preset. */
export function cameraModeFps(mode: CameraMode): number {
  if (mode.frameRateDenominator === 0) return 0;
  return Math.round(
    (mode.frameRateNumerator / mode.frameRateDenominator) * 10
  ) / 10;
}

/** Whether any enumerated mode matches `width`x`height`. */
export function cameraSupportsResolution(
  modes: CameraMode[],
  width: number,
  height: number
): boolean {
  return modes.some((mode) => mode.width === width && mode.height === height);
}

/** Whether any enumerated mode at `width`x`height` runs at (rounded) `fps`. */
export function cameraSupportsFps(
  modes: CameraMode[],
  width: number,
  height: number,
  fps: number
): boolean {
  return modes.some(
    (mode) =>
      mode.width === width &&
      mode.height === height &&
      Math.round(cameraModeFps(mode)) === fps
  );
}

export async function listCameraModes(
  deviceId?: string
): Promise<CameraMode[] | null> {
  if (!hasTauriBridge()) return null;
  return await invoke<CameraMode[]>(COMMANDS.listCameraModes, {
    preferredDeviceId: deviceId ?? null
  });
}

export async function setCameraPrefs(
  width: number | null,
  height: number | null,
  frameRate: number | null
): Promise<AppliedCameraDevice | null> {
  if (!hasTauriBridge()) return null;
  return await invoke<AppliedCameraDevice>(COMMANDS.setCameraPrefs, {
    width,
    height,
    frameRate
  });
}

export async function seedCameraModePreference(): Promise<void> {
  if (!hasTauriBridge() || !session.cameraMode) return;
  try {
    await setCameraPrefs(
      session.cameraMode.width,
      session.cameraMode.height,
      session.cameraMode.frameRate
    );
  } catch (e) {
    console.warn('seedCameraModePreference failed', e);
  }
}
