import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { friendlyUpdateErrorMessage } from '../src/lib/data/updaterErrors.ts';

const updaterSource = readFileSync(new URL('../src/lib/updater.ts', import.meta.url), 'utf8');
const layoutSource = readFileSync(new URL('../src/routes/+layout.svelte', import.meta.url), 'utf8');
const ipcSource = readFileSync(new URL('../src/lib/ipc.ts', import.meta.url), 'utf8');
const loggingSource = readFileSync(new URL('../src-tauri/src/logging.rs', import.meta.url), 'utf8');
const libSource = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');
const updateStatusSource = readFileSync(
  new URL('../src/lib/stores/updateStatus.svelte.ts', import.meta.url),
  'utf8'
);
const toastHostSource = readFileSync(
  new URL('../src/lib/components/ToastHost.svelte', import.meta.url),
  'utf8'
);

test('updater frontend steps are bridged into petal.log', () => {
  assert.match(ipcSource, /logUpdaterEvent: 'log_updater_event'/);
  assert.match(loggingSource, /pub fn log_updater_event/);
  assert.match(loggingSource, /log::info!\("updater: \{message\}"\)/);
  assert.match(loggingSource, /log::error!\("updater: \{message\}"\)/);
  assert.match(libSource, /logging::log_updater_event/);

  assert.match(updaterSource, /invoke\(COMMANDS\.logUpdaterEvent, \{ level, message \}\)/);
  assert.match(updaterSource, /COMMANDS\.checkCompatibleUpdateAvailable/);
  assert.match(updaterSource, /COMMANDS\.downloadAndInstallCompatibleUpdate/);
  assert.match(updaterSource, /check start/);
  assert.match(updaterSource, /up to date/);
  assert.match(updaterSource, /checking availability/);
  assert.match(updaterSource, /installed; relaunching/);
  assert.match(updaterSource, /waiting for explicit restart/);
  assert.match(updaterSource, /failed: \$\{rawMessage\}/);
});

test('updater failures surface as a degraded visible toast', () => {
  assert.match(updateStatusSource, /\| \{ kind: 'failed'; message: string \}/);
  assert.match(updateStatusSource, /markUpdateFailed\(message: string\)/);
  assert.match(updaterSource, /markUpdateFailed\(friendlyMessage\)/);
  // The raw error (which can be an arbitrarily long/technical string, e.g. a
  // temp-file path) must never reach the toast directly -- see #105/the
  // AppleDouble incident, where it did and broke the layout.
  assert.match(updaterSource, /import \{ friendlyUpdateErrorMessage \} from '\$lib\/data\/updaterErrors'/);
  assert.match(toastHostSource, /Update check failed: \$\{updateStatus\.message\}/);
  assert.match(toastHostSource, /variant=\{updateStatus\.kind === 'failed' \? 'degraded' : 'info'\}/);
});

test('friendlyUpdateErrorMessage never leaks raw/long error text into the UI (#105)', () => {
  // The exact incident: a raw archive-unpack failure carrying a full
  // temp-file path broke the toast layout (a 6-line pill spilling past the
  // window). Every category must collapse to one short, fixed sentence.
  const rawUnpackError =
    'failed to unpack `._Petal.app` into `/var/folders/hv/1j1bc2z94gb82jlgzksty/T/tauri_updated_app170.tmp`: some very long underlying OS error text that keeps going and going';
  assert.equal(
    friendlyUpdateErrorMessage(rawUnpackError),
    "Couldn't install the update — try again later"
  );
  assert.equal(
    friendlyUpdateErrorMessage('signature verification failed for bundle'),
    'Update failed a security check and was rejected'
  );
  assert.equal(
    friendlyUpdateErrorMessage('update architecture x86_64 not supported by this build'),
    "This build isn't compatible with your Mac"
  );
  assert.equal(
    friendlyUpdateErrorMessage('network error: dns lookup timed out during fetch'),
    "Couldn't reach the update server — check your connection"
  );
  assert.equal(
    friendlyUpdateErrorMessage('some completely unrecognized error shape'),
    'Update check failed — see logs for details'
  );

  // No matter the input, the output must stay short enough to never
  // reproduce the overflow -- a hard length ceiling, not just "usually short".
  const allOutputs = [
    rawUnpackError,
    'signature verification failed for bundle',
    'update architecture x86_64 not supported by this build',
    'network error: dns lookup timed out during fetch',
    'x'.repeat(5000),
  ].map(friendlyUpdateErrorMessage);
  for (const output of allOutputs) {
    assert.ok(output.length <= 60, `expected a short summary, got ${output.length} chars: ${output}`);
  }
});

test('a macOS install failure reaches the user as something they can act on (#871)', () => {
  // Before #871 every one of these collapsed to "Update check failed — see
  // logs for details": the Rust side already knew the user just had to move
  // Petal out of a disk image, and the UI threw that away. Each raw string
  // below is the real text `mac_install_user_message` produces.
  const cases: Array<[string, string]> = [
    [
      'Petal is running from a read-only disk image. Drag Petal into Applications, then try again.',
      'move Petal to Applications, then update',
    ],
    [
      'Petal could not install the update because /Volumes/Petal 0.9.0 and the staging folder are on different disks.',
      'move Petal to Applications, then update',
    ],
    [
      'This update needs an administrator password. Moving Petal to Applications avoids this.',
      'an administrator password is needed',
    ],
    [
      'The update could not be completed. Your previous Petal is safe at /Applications/.petal-update-123/old -- move it back to /Applications/Petal.app.',
      'see the logs to restore your previous Petal',
    ],
  ];

  for (const [raw, expected] of cases) {
    const friendly = friendlyUpdateErrorMessage(raw);
    assert.equal(friendly, expected);
    // The #105 ceiling still binds, and no path may survive into the toast.
    assert.ok(friendly.length <= 60, `expected a short summary, got ${friendly.length} chars`);
    assert.ok(!friendly.includes('/'), `a path leaked into the toast: ${friendly}`);
  }
});

test('updater checks on launch and main-menu entry with launch bypassing throttle', () => {
  assert.match(layoutSource, /runUpdateCheck\('launch', \{ force: true \}\)/);
  assert.match(layoutSource, /runUpdateCheck\('main-menu'\)/);
  assert.match(layoutSource, /if \(!opts\.force && now - lastUpdateCheckMs < UPDATE_CHECK_THROTTLE_MS\) return null/);
  assert.match(layoutSource, /checkForUpdate\(\{ skipRelaunch: true, reason \}\)/);
});

test('launch update check is main-window-only and once-per-process', () => {
  // Secondary windows and hard navigations mount the same root layout; the
  // launch check must be gated to the main window and latched in Rust.
  assert.match(layoutSource, /getCurrentWindow\(\)\.label === 'main'/);
  assert.match(updaterSource, /COMMANDS\.runLaunchUpdateCheck/);
  assert.match(updaterSource, /if \(launchResult === null\)/);
  assert.match(libSource, /updater::run_launch_update_check/);
});

test('passive updater checks never stage an update; restart action installs explicitly (#113)', () => {
  const checkFunction = updaterSource.slice(
    updaterSource.indexOf('export async function checkForUpdate'),
    updaterSource.indexOf('export async function installUpdateAndRelaunch')
  );
  const installFunction = updaterSource.slice(
    updaterSource.indexOf('export async function installUpdateAndRelaunch')
  );

  assert.match(checkFunction, /COMMANDS\.checkCompatibleUpdateAvailable/);
  assert.doesNotMatch(checkFunction, /COMMANDS\.downloadAndInstallCompatibleUpdate/);
  assert.doesNotMatch(checkFunction, /relaunch\(\)/);
  assert.match(checkFunction, /markUpdateAvailable\(version\)/);

  assert.match(installFunction, /COMMANDS\.downloadAndInstallCompatibleUpdate/);
  assert.match(installFunction, /await relaunch\(\)/);
  assert.match(toastHostSource, /installUpdateAndRelaunch\('toast'\)/);
  assert.match(toastHostSource, /updateStatus\.kind === 'available'/);
  assert.match(toastHostSource, /restart to install/);
});
