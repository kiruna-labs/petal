// #172: where this launch runs from, and what the launch router does about it.
//
// Wraps `launch_location_class` (src-tauri/src/launch_location.rs). The class
// is a closed string, never a path. A run from the mounted disk image or an
// App-Translocated bundle cannot update itself (the updater's install step
// fails on the read-only/ephemeral location), so the router diverts those
// launches to /relocate BEFORE the onboarding/main decision -- a returning,
// fully-permissioned user would otherwise go straight to /main and never see
// it.
import { invoke } from '@tauri-apps/api/core';
import { COMMANDS } from '../ipc';

export const LAUNCH_LOCATION_CLASSES = [
  'applications',
  'user_applications',
  'disk_image',
  'translocated',
  'other_read_only',
  'other',
  'unbundled'
] as const;
export type LaunchLocationClass = (typeof LAUNCH_LOCATION_CLASSES)[number];

export function isLaunchLocationClass(value: unknown): value is LaunchLocationClass {
  return typeof value === 'string' && (LAUNCH_LOCATION_CLASSES as readonly string[]).includes(value);
}

/** The classes for which the updater cannot install in place. */
export function needsRelocation(cls: LaunchLocationClass): boolean {
  return cls === 'disk_image' || cls === 'translocated' || cls === 'other_read_only';
}

/**
 * Ask the backend. Any failure (no bridge, unknown value) reads as `other`:
 * the notice must never block a launch on a broken probe.
 */
export async function fetchLaunchLocationClass(): Promise<LaunchLocationClass> {
  try {
    const value = await invoke<string>(COMMANDS.launchLocationClass);
    return isLaunchLocationClass(value) ? value : 'other';
  } catch (e) {
    console.warn('launch: launch_location_class unavailable', e);
    return 'other';
  }
}

export interface LaunchRouteInput {
  onboardingComplete: boolean;
  hasBridge: boolean;
  /** `null` when there is no bridge to ask. */
  location: LaunchLocationClass | null;
  /** Evaluated only when the bridge is present. */
  permissionsOk: boolean;
}

export type LaunchRoute = '/relocate' | '/onboarding' | '/main';

/**
 * The root route's decision, in order:
 * 1. a location that needs relocation wins over everything (a fully set-up
 *    returning user must still see it);
 * 2. onboarding never completed -> /onboarding;
 * 3. no bridge -> /main (browser fallback);
 * 4. a required permission missing -> /onboarding; else /main.
 */
export function launchRoute(input: LaunchRouteInput): LaunchRoute {
  if (input.location !== null && needsRelocation(input.location)) return '/relocate';
  if (!input.onboardingComplete) return '/onboarding';
  if (!input.hasBridge) return '/main';
  return input.permissionsOk ? '/main' : '/onboarding';
}

/** Copy for the /relocate notice, per class. Every string must fit the
 * 400px main window untruncated; the detail wraps. */
export function relocateCopy(cls: LaunchLocationClass): {
  title: string;
  detail: string;
  showReveal: boolean;
} {
  switch (cls) {
    case 'disk_image':
      return {
        title: 'Move Petal to Applications',
        detail:
          "Petal is running from the disk image, so it can't update itself. Drag Petal into your Applications folder, eject the disk image, then open Petal from Applications.",
        showReveal: true
      };
    case 'translocated':
      return {
        title: 'Move Petal to Applications',
        detail:
          "Petal is running from a temporary location macOS created for a downloaded app, so it can't update itself. Move Petal into your Applications folder, then open it from there.",
        showReveal: false
      };
    default:
      return {
        title: 'Move Petal to Applications',
        detail:
          "Petal is running from a read-only location, so it can't update itself. Move Petal into your Applications folder, then open it from there.",
        showReveal: false
      };
  }
}

export async function openApplicationsFolder(): Promise<void> {
  try {
    await invoke(COMMANDS.openApplicationsFolder);
  } catch (e) {
    console.warn('launch: open_applications_folder failed', e);
  }
}

export async function revealRunningBundle(): Promise<void> {
  try {
    await invoke(COMMANDS.revealRunningBundle);
  } catch (e) {
    console.warn('launch: reveal_running_bundle failed', e);
  }
}
