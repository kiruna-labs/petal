// Real permission client (SPEC.md §4.1) — wraps the Tauri commands in
// src-tauri/src/permissions.rs. Replaces the old "Simulate granting …" mock
// buttons on the onboarding route with real OS-permission checks/requests for
// Screen Recording, Microphone, Camera, and Accessibility.
//
// Graceful degradation: a plain browser preview (no `__TAURI_INTERNALS__`
// bridge) has no backend, so every call is wrapped so callers can `.catch()`
// and fall back to a sensible default rather than crashing `npm run check` /
// the preview. Mirrors the existing `invoke().catch()` pattern used across the
// codebase (see `rooms.ts`, `/main`, etc.).
import { invoke } from '@tauri-apps/api/core';
import { openUrl } from '@tauri-apps/plugin-opener';
import { COMMANDS } from '$lib/ipc';
import type { AuthStatus, PermissionRequestOutcome } from '$lib/ipc';
export type { AuthStatus, PermissionRequestOutcome } from '$lib/ipc';

/**
 * The four `AVAuthorizationStatus` values, as returned by the Rust
 * `check_*`/`request_*` mic/camera commands (mirrors
 * `permissions::auth_status_string`).
 */

/** System Settings Privacy deep-links (SPEC.md §4.1). */
export const SETTINGS_URLS = {
  screenRecording:
    'x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture',
  microphone: 'x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone',
  camera: 'x-apple.systempreferences:com.apple.preference.security?Privacy_Camera',
  // Remote control replays input via CGEvent, which needs Accessibility (#201).
  accessibility:
    'x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility'
} as const;

// --- Screen Recording -------------------------------------------------------

/** Preflight check — no prompt. Returns false if the backend isn't present. */
export async function checkScreenRecording(): Promise<boolean> {
  try {
    return await invoke<boolean>(COMMANDS.checkScreenRecording);
  } catch (e) {
    console.warn('checkScreenRecording: no Tauri backend, defaulting to false', e);
    return false;
  }
}

/**
 * Triggers the OS Screen Recording prompt. Returns the immediate result, but
 * the caller MUST relaunch after a fresh grant — macOS only re-reads this
 * grant at process start (see permissions.rs's module doc comment).
 */
export async function requestScreenRecording(): Promise<PermissionRequestOutcome> {
  try {
    return await invoke<PermissionRequestOutcome>(COMMANDS.requestScreenRecording);
  } catch (e) {
    console.warn('requestScreenRecording: no Tauri backend', e);
    return { granted: false, wasGranted: false, autoRelaunchRecommended: false };
  }
}

// --- Microphone -------------------------------------------------------------

export async function checkMicrophone(): Promise<AuthStatus> {
  try {
    return await invoke<AuthStatus>(COMMANDS.checkMicrophone);
  } catch (e) {
    console.warn('checkMicrophone: no Tauri backend, defaulting to not-determined', e);
    return 'not-determined';
  }
}

export async function requestMicrophone(): Promise<AuthStatus> {
  try {
    return await invoke<AuthStatus>(COMMANDS.requestMicrophone);
  } catch (e) {
    console.warn('requestMicrophone: no Tauri backend', e);
    return 'not-determined';
  }
}

// --- Camera -----------------------------------------------------------------

export async function checkCamera(): Promise<AuthStatus> {
  try {
    return await invoke<AuthStatus>(COMMANDS.checkCamera);
  } catch (e) {
    console.warn('checkCamera: no Tauri backend, defaulting to not-determined', e);
    return 'not-determined';
  }
}

export async function requestCamera(): Promise<AuthStatus> {
  try {
    return await invoke<AuthStatus>(COMMANDS.requestCamera);
  } catch (e) {
    console.warn('requestCamera: no Tauri backend', e);
    return 'not-determined';
  }
}

// --- Accessibility ----------------------------------------------------------

/** Preflight check — no prompt. Required for replaying remote-control input. */
export async function checkAccessibility(): Promise<boolean> {
  try {
    return await invoke<boolean>(COMMANDS.checkAccessibility);
  } catch (e) {
    console.warn('checkAccessibility: no Tauri backend, defaulting to false', e);
    return false;
  }
}

/** Registers Petal in the macOS Accessibility list and shows the system prompt. */
export async function requestAccessibility(): Promise<PermissionRequestOutcome> {
  try {
    return await invoke<PermissionRequestOutcome>(COMMANDS.requestAccessibility);
  } catch (e) {
    console.warn('requestAccessibility: no Tauri backend', e);
    return { granted: false, wasGranted: false, autoRelaunchRecommended: false };
  }
}

/**
 * Camera TCC gate (issue #8): check first, and only when the OS has never
 * been asked (`not-determined`) trigger the real AVCaptureDevice prompt via
 * `request_camera`. Never re-prompts on `denied`/`restricted` — macOS wouldn't
 * show the dialog again anyway; callers must route those to the System
 * Settings recovery path instead of calling getUserMedia (which fails
 * instantly with NotAllowedError when TCC isn't authorized).
 *
 * In a plain browser preview (no Tauri backend) both wrapped calls fall back
 * to 'not-determined', so callers treating only 'denied'/'restricted' as
 * blocking keep working there (the browser's own getUserMedia prompt takes
 * over).
 */
export async function ensureCameraAccess(): Promise<AuthStatus> {
  const status = await checkCamera();
  if (status === 'not-determined') {
    return await requestCamera();
  }
  return status;
}

// --- System Settings deep-links ---------------------------------------------

/**
 * Open the relevant System Settings Privacy pane so the user can flip a
 * DENIED permission back on (SPEC.md §4.1). Prefer the native command because
 * it logs the exact pane and uses macOS's `open` binary for x-apple URLs; keep
 * the plugin opener as a browser/dev fallback.
 */
export async function openPrivacySettings(
  which: keyof typeof SETTINGS_URLS
): Promise<void> {
  try {
    console.info(`openPrivacySettings(${which}): requesting native settings opener`);
    const opened = await invoke<boolean>(COMMANDS.openPrivacySettings, { which });
    if (opened) {
      return;
    }
    console.warn(`openPrivacySettings(${which}): native opener returned false, falling back`);
  } catch (e) {
    console.warn(`openPrivacySettings(${which}): native opener failed, falling back`, e);
  }

  try {
    await openUrl(SETTINGS_URLS[which]);
  } catch (e) {
    console.warn(`openPrivacySettings(${which}): failed to open`, e);
  }
}

/** Restart Petal so macOS TCC grants that only apply at process start take effect. */
export async function restartApp(reason = 'permission-recovery'): Promise<boolean> {
  try {
    return await invoke<boolean>(COMMANDS.restartApp, { reason });
  } catch (e) {
    console.warn('restartApp: failed to request app restart', e);
    return false;
  }
}
