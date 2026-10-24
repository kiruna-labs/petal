import { invoke } from '@tauri-apps/api/core';
import { session } from '#lib/stores/session.svelte.js';
import { COMMANDS, hasTauriBridge } from '#lib/ipc.js';
import type { AppliedCameraDevice, CameraDeviceInfo } from '#lib/ipc.js';

export { hasTauriBridge } from '#lib/ipc.js';
export type { AppliedCameraDevice, CameraDeviceInfo } from '#lib/ipc.js';

export async function listCameraDevices(): Promise<CameraDeviceInfo[] | null> {
  if (!hasTauriBridge()) return null;
  return await invoke<CameraDeviceInfo[]>(COMMANDS.listCameraDevices);
}

export async function setCameraDevice(deviceId: string): Promise<AppliedCameraDevice | null> {
  if (!hasTauriBridge()) return null;
  return await invoke<AppliedCameraDevice>(COMMANDS.setCameraDevice, { deviceId });
}

export async function seedCameraDevicePreference(): Promise<void> {
  if (!hasTauriBridge() || !session.cameraDeviceId) return;
  try {
    await setCameraDevice(session.cameraDeviceId);
  } catch (e) {
    console.warn('seedCameraDevicePreference failed', e);
  }
}
