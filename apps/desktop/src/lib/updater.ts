// Silent auto-update (issue #103, the production plan "make it updatable").
//
// On app startup the frontend calls `checkForUpdate()` once. It asks
// the Rust updater command to hit `plugins.updater.endpoints`
// (tauri.release.conf.json -> our Vercel `/api/updater`, which serves the
// CI-published `latest.json` from Vercel Blob; the committed tauri.conf.json
// ships an empty list, so an open-source build reports up-to-date with no
// network request at all). If the user explicitly clicks
// Restart now, Rust downloads the minisign-verified `.app.tar.gz` and verifies
// the staged app bundle contains the running CPU architecture before
// install/relaunch (#90).
//
// This is deliberately conservative about WHEN it acts:
//   - Only in a real Tauri context (no-op in a plain browser / `npm run dev`
//     preview, where there is no updater host — `isTauri()` guards it and any
//     residual error is swallowed, never surfaced as an app crash).
//   - Passive checks never download or install. A user must click Restart now
//     before the app stages the replacement bundle, so an ordinary quit/reopen
//     cannot silently apply a previously found update (#113).
//
// There is intentionally no dialog/prompt here -- updater activity is surfaced
// through the existing in-webview ToastHost and otherwise stays quiet.

import { invoke, isTauri } from '@tauri-apps/api/core';
import { COMMANDS } from '$lib/ipc';
import {
  clearUpdateStatus,
  markUpdateDownloading,
  markUpdateAvailable,
  markUpdateFailed,
  markUpdateRelaunching
} from '$lib/stores/updateStatus.svelte';
import { friendlyUpdateErrorMessage } from '$lib/data/updaterErrors';

export interface UpdateResult {
  status: 'up-to-date' | 'available' | 'installed' | 'unavailable' | 'error';
  version?: string;
  error?: string;
}

type UpdaterLogLevel = 'info' | 'warn' | 'error';

async function logUpdaterStep(level: UpdaterLogLevel, message: string): Promise<void> {
  const line = `[updater] ${message}`;
  if (level === 'error') {
    console.error(line);
  } else if (level === 'warn') {
    console.warn(line);
  } else {
    console.info(line);
  }

  try {
    await invoke(COMMANDS.logUpdaterEvent, { level, message });
  } catch (err) {
    console.warn('[updater] failed to write updater step to petal.log', err);
  }
}

function updateErrorMessage(err: unknown): string {
  const message = err instanceof Error ? err.message : String(err);
  return message.replace(/\s+/g, ' ').trim() || 'unknown error';
}

/**
 * Check for an update. This intentionally does not download or install:
 * installing stages the replacement app and macOS/Tauri can apply that on an
 * ordinary future relaunch, which makes a passive check behave like a silent
 * auto-update (#113).
 *
 * @param opts.skipRelaunch  legacy no-op kept for older callers; checks are
 *   always non-installing now.
 */
export async function checkForUpdate(
  opts: { skipRelaunch?: boolean; reason?: 'launch' | 'main-menu' | 'manual' } = {}
): Promise<UpdateResult> {
  // No updater host outside a bundled Tauri app. Bail quietly so the browser
  // preview / dev server never logs a scary error.
  if (!isTauri()) {
    clearUpdateStatus();
    return { status: 'unavailable' };
  }

  try {
    // The passive launch check is once-per-process: the Rust command latches
    // and returns null for any later webview mount (window picker, network
    // cockpit) or hard navigation (deep-link meeting join) that remounts the
    // root layout. Claim it before any logging so repeat mounts are silent.
    if (opts.reason === 'launch') {
      const launchResult = await invoke<{
        status: 'up-to-date' | 'available';
        version: string | null;
      } | null>(COMMANDS.runLaunchUpdateCheck, {});
      if (launchResult === null) {
        clearUpdateStatus();
        return { status: 'up-to-date' };
      }
      await logUpdaterStep('info', 'check start (launch)');
      if (launchResult.status === 'up-to-date') {
        clearUpdateStatus();
        await logUpdaterStep('info', 'up to date');
        return { status: 'up-to-date' };
      }
      const launchVersion = launchResult.version ?? undefined;
      markUpdateAvailable(launchVersion);
      await logUpdaterStep(
        'info',
        `available${launchVersion ? ` ${launchVersion}` : ''}; waiting for explicit restart`
      );
      return { status: 'available', version: launchVersion };
    }

    await logUpdaterStep('info', `check start (${opts.reason ?? 'manual'})`);

    markUpdateDownloading();
    await logUpdaterStep('info', 'checking availability');
    const result = await invoke<{ status: 'up-to-date' | 'available'; version: string | null }>(
      COMMANDS.checkCompatibleUpdateAvailable,
      {}
    );

    if (result.status === 'up-to-date') {
      clearUpdateStatus();
      await logUpdaterStep('info', 'up to date');
      return { status: 'up-to-date' };
    }

    const version = result.version ?? undefined;
    markUpdateAvailable(version);
    await logUpdaterStep('info', `available${version ? ` ${version}` : ''}; waiting for explicit restart`);
    return { status: 'available', version };
  } catch (err) {
    // Never let an updater failure (offline, endpoint down, install/signature
    // rejection) crash the app, but do make it observable in petal.log and in
    // the UI so failure is no longer indistinguishable from "up to date" (#43).
    // The full raw message (which can be an arbitrarily long technical string,
    // e.g. a temp-file path from a failed unpack) goes to the log only; the
    // UI (toast + Settings' manual check) gets a short, fixed-length summary
    // so a verbose error can never break the toast layout.
    const rawMessage = updateErrorMessage(err);
    const friendlyMessage = friendlyUpdateErrorMessage(rawMessage);
    markUpdateFailed(friendlyMessage);
    await logUpdaterStep('error', `failed: ${rawMessage}`);
    return { status: 'error', error: friendlyMessage };
  }
}

export async function installUpdateAndRelaunch(
  reason: 'toast' | 'manual' = 'toast'
): Promise<UpdateResult> {
  if (!isTauri()) {
    clearUpdateStatus();
    return { status: 'unavailable' };
  }

  try {
    await logUpdaterStep('info', `explicit install start (${reason})`);

    markUpdateDownloading();
    await logUpdaterStep('info', 'downloading');
    const result = await invoke<{ status: 'up-to-date' | 'installed'; version: string | null }>(
      COMMANDS.downloadAndInstallCompatibleUpdate,
      {}
    );

    if (result.status === 'up-to-date') {
      clearUpdateStatus();
      await logUpdaterStep('info', 'up to date before explicit install');
      return { status: 'up-to-date' };
    }

    markUpdateRelaunching();
    await logUpdaterStep('info', 'installed; relaunching');
    const { relaunch } = await import('@tauri-apps/plugin-process');
    await relaunch();
    // relaunch() replaces the process; nothing after this runs.
    return { status: 'installed', version: result.version ?? undefined };
  } catch (err) {
    const rawMessage = updateErrorMessage(err);
    const friendlyMessage = friendlyUpdateErrorMessage(rawMessage);
    markUpdateFailed(friendlyMessage);
    await logUpdaterStep('error', `failed: ${rawMessage}`);
    return { status: 'error', error: friendlyMessage };
  }
}
