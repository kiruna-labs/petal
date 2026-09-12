#!/usr/bin/env node
//
// measure-camera-intent.mjs -- run #76's runbook against a live Petal and read
// the result out of petal.log, as one command.
//
// The runbook: with the Settings camera preview live, join a meeting, toggle
// the meeting camera ON, then OFF, then switch camera device with the preview
// still open, and read the camera-intent margin from the log. Every step is
// driven through the debug command socket (`autotest.rs`), so what is left for
// a person is starting a debug build on a Mac with a camera:
//
//   cd apps/desktop
//   PETAL_AUTOTEST_SOCK=/tmp/petal-76.sock \
//   PETAL_AUTOTEST_ROOM="$PETAL_TEST_QA_KEY" \
//   PETAL_AUTOTEST_IDENTITY="$(uuidgen | tr 'A-Z' 'a-z')" \
//     npm run dev:clean
//   node scripts/measure-camera-intent.mjs --socket /tmp/petal-76.sock
//
// What it needs and why it cannot run on the VM runner: a REAL camera, held
// by a REAL Settings preview. The Tart guest has neither, and the synthetic
// camera source contends for no device.
//
// What it checks along the way: that the preview actually went live before
// the first toggle (the `settings: camera preview acquired` line, without
// which a first-attempt win is not evidence about contention), and that
// every toggle reached a terminal state before the next one. It then hands
// the log to scripts/analyze-field-log.mjs, whose output is the deliverable:
// numbers, stages and verdicts, nothing from the log itself.

import { readFileSync, readdirSync, statSync, openSync, readSync, closeSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import process from 'node:process';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { spawnSync } from 'node:child_process';

import { connectSocket } from './autotest-socket.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, '..', '..', '..');
const ANALYZER = join(REPO_ROOT, 'scripts', 'analyze-field-log.mjs');

const PREVIEW_ACQUIRED = 'settings: camera preview acquired';
const PREVIEW_RELEASED = 'settings: camera preview released';

const USAGE = `Usage: node scripts/measure-camera-intent.mjs [--socket <path>] [options]

Drives #76's runbook (Settings preview live -> meeting camera ON -> OFF ->
device switch) over the autotest socket of a running DEBUG Petal, then reads
the camera-intent margin out of petal.log with scripts/analyze-field-log.mjs.

  --socket <path>     autotest socket (default: $PETAL_AUTOTEST_SOCK)
  --log <file>        petal.log to read (default: newest in ~/Library/Logs/Petal)
  --room <qa-key>     join this room first if the app is not in one
  --settle-ms <n>     pause between toggles (default 2500)
  --preview-timeout-ms <n>  how long to wait for the preview to go live (default 20000)
  --keep-settings     leave the Settings window open afterwards
  --json              pass --json to the analyzer
  --no-analyze        drive the runbook only; skip the analyzer

Needs a Mac with a real camera, and the app launched with PETAL_AUTOTEST_SOCK
(and PETAL_AUTOTEST_ROOM, or --room). See docs/TESTING.md, "Desktop Autotest
Scenarios".`;

function parseArgs(argv) {
  const options = {
    socket: process.env.PETAL_AUTOTEST_SOCK || null,
    log: null,
    room: null,
    settleMs: 2500,
    previewTimeoutMs: 20_000,
    keepSettings: false,
    json: false,
    analyze: true,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const next = () => {
      i += 1;
      if (i >= argv.length) throw new Error(`${arg} needs a value`);
      return argv[i];
    };
    if (arg === '--socket') options.socket = next();
    else if (arg === '--log') options.log = next();
    else if (arg === '--room') options.room = next();
    else if (arg === '--settle-ms') options.settleMs = Number(next());
    else if (arg === '--preview-timeout-ms') options.previewTimeoutMs = Number(next());
    else if (arg === '--keep-settings') options.keepSettings = true;
    else if (arg === '--json') options.json = true;
    else if (arg === '--no-analyze') options.analyze = false;
    else if (arg === '--help' || arg === '-h') options.help = true;
    else throw new Error(`unknown argument ${arg}`);
  }
  if (!Number.isFinite(options.settleMs) || options.settleMs < 0) {
    throw new Error('--settle-ms must be a non-negative number');
  }
  if (!Number.isFinite(options.previewTimeoutMs) || options.previewTimeoutMs < 0) {
    throw new Error('--preview-timeout-ms must be a non-negative number');
  }
  return options;
}

// ---------------------------------------------------------------------------
// Socket client: the shared BOUNDED one (#102) -- `camera_on` blocks the
// app's socket thread for the whole publish attempt, and an app that stops
// answering must fail this run loudly rather than hang it.
// ---------------------------------------------------------------------------

/** Command-response ceiling: a publish attempt waits up to first-frame
 *  timeout plus a self-heal handoff, well under this. */
const COMMAND_TIMEOUT_MS = 60_000;

class Socket {
  constructor(path) {
    this.client = connectSocket(path, { timeoutMs: COMMAND_TIMEOUT_MS });
  }

  /** Send, and throw with the socket's own error text when it refuses. */
  async ok(command) {
    const response = await this.client.send(command);
    if (!response.ok) throw new Error(`${command.cmd}: ${response.error}`);
    return response.result ?? null;
  }

  close() {
    this.client.close();
  }
}

// ---------------------------------------------------------------------------
// Log tail: only "did a line containing this marker land after this point"
// -- no line is ever echoed.
// ---------------------------------------------------------------------------

function newestPetalLog() {
  const dir = join(homedir(), 'Library', 'Logs', 'Petal');
  let names;
  try {
    names = readdirSync(dir);
  } catch {
    throw new Error(`no log directory at ${dir}; pass --log`);
  }
  const candidates = names
    .filter((name) => /^petal\.log(\.\d{4}-\d{2}-\d{2})?$/.test(name))
    .map((name) => ({ name, mtimeMs: statSync(join(dir, name)).mtimeMs }))
    .sort((a, b) => b.mtimeMs - a.mtimeMs);
  if (candidates.length === 0) throw new Error(`no petal.log in ${dir}; pass --log`);
  return join(dir, candidates[0].name);
}

class LogTail {
  constructor(path) {
    this.path = path;
    this.offset = statSync(path).size;
    this.partial = '';
  }

  /** New complete lines since the last call. */
  drain() {
    const size = statSync(this.path).size;
    if (size < this.offset) {
      // Truncated or rotated under us; start over from the top of the new file.
      this.offset = 0;
      this.partial = '';
    }
    if (size === this.offset) return [];
    const length = size - this.offset;
    const buffer = Buffer.alloc(length);
    const fd = openSync(this.path, 'r');
    try {
      readSync(fd, buffer, 0, length, this.offset);
    } finally {
      closeSync(fd);
    }
    this.offset = size;
    const text = this.partial + buffer.toString('utf8');
    const lines = text.split('\n');
    this.partial = lines.pop() ?? '';
    return lines;
  }

  async waitFor(marker, timeoutMs) {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      if (this.drain().some((line) => line.includes(marker))) return true;
      if (Date.now() >= deadline) return false;
      await sleep(150);
    }
  }
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function pollUntil(describe, probe, predicate, timeoutMs, intervalMs = 200) {
  const deadline = Date.now() + timeoutMs;
  let last;
  for (;;) {
    last = await probe();
    if (predicate(last)) return last;
    if (Date.now() >= deadline) {
      throw new Error(`${describe}: timed out after ${timeoutMs} ms (last: ${JSON.stringify(last)})`);
    }
    await sleep(intervalMs);
  }
}

// ---------------------------------------------------------------------------
// The runbook.
// ---------------------------------------------------------------------------

function step(text) {
  console.log(`-- ${text}`);
}

function warn(text) {
  console.log(`!! ${text}`);
}

async function cameraState(socket) {
  return socket.ok({ cmd: 'camera_state' });
}

async function waitPublishing(socket, publishing, describe) {
  return pollUntil(
    describe,
    () => cameraState(socket),
    (state) => state.publishing === publishing || (publishing && state.intended === false),
    30_000
  ).then((state) => {
    if (publishing && !state.publishing) {
      throw new Error(`${describe}: the intent was cleared before a track published`);
    }
    return state;
  });
}

async function runbook(options) {
  const socket = new Socket(options.socket);
  const findings = { previewLive: false, deviceSwitch: 'not attempted', joinedHere: false };
  try {
    // 1. A room. The socket join takes the same QA key PETAL_AUTOTEST_ROOM
    // does; without either the app cannot publish anything.
    let state;
    try {
      state = await socket.ok({ cmd: 'dump_state' });
    } catch (error) {
      throw new Error(
        `cannot talk to ${options.socket} (${error.message}). Launch a DEBUG Petal with ` +
          'PETAL_AUTOTEST_SOCK set to that path first.'
      );
    }
    if (!state.currentRoom) {
      if (!options.room) {
        throw new Error(
          'the app is not in a room. Launch with PETAL_AUTOTEST_ROOM, or pass --room <qa-key>.'
        );
      }
      step('joining the room');
      await socket.ok({ cmd: 'join', room: options.room });
      findings.joinedHere = true;
      state = await pollUntil(
        'join',
        () => socket.ok({ cmd: 'dump_state' }),
        (s) => Boolean(s.currentRoom),
        30_000
      );
    }
    step('in a room');

    // 2. Start from the camera OFF, so the first toggle is a real ON.
    const initial = await cameraState(socket);
    if (initial.publishing || initial.intended) {
      step('meeting camera was on; turning it off first');
      await socket.ok({ cmd: 'camera_off' });
      await waitPublishing(socket, false, 'initial camera_off');
    }

    // 3. Settings, with the preview live. The preview's own log line is the
    // only evidence it holds the device; without it the run is NOT the
    // runbook and the analyzer will say so.
    const tail = new LogTail(options.log);
    step('opening Settings and waiting for the camera preview to go live');
    await socket.ok({ cmd: 'open_settings' });
    findings.previewLive = await tail.waitFor(PREVIEW_ACQUIRED, options.previewTimeoutMs);
    if (!findings.previewLive) {
      warn(
        `no \`settings: camera preview acquired\` line within ${options.previewTimeoutMs} ms. ` +
          'Either the camera permission ' +
          'is not granted to this build (grant it in the Settings window that just opened and ' +
          're-run), the build predates the preview line, or --log points at the wrong file. ' +
          'Continuing, but the episodes below will be UNCONTENDED or UNKNOWN.'
      );
    }
    await sleep(options.settleMs);

    // 4. ON.
    step('meeting camera ON');
    const on = await socket.ok({ cmd: 'camera_on' });
    if (!on.published) step('  the immediate attempt did not publish; waiting on the self-heal loop');
    await waitPublishing(socket, true, 'camera_on');
    await sleep(options.settleMs);

    // 5. OFF, and the preview coming back.
    step('meeting camera OFF');
    await socket.ok({ cmd: 'camera_off' });
    await waitPublishing(socket, false, 'camera_off');
    if (findings.previewLive && !(await tail.waitFor(PREVIEW_ACQUIRED, 15_000))) {
      warn('the preview did not come back within 15 s of the camera going off');
    }
    await sleep(options.settleMs);

    // 6. Device switch while the preview is open: ON again, switch, OFF.
    const { devices } = await socket.ok({ cmd: 'list_camera_devices' });
    if (devices.length < 2) {
      findings.deviceSwitch = `skipped (${devices.length} camera device on this Mac; a switch needs two)`;
      warn(findings.deviceSwitch);
    } else {
      step('meeting camera ON again, for the device switch');
      await socket.ok({ cmd: 'camera_on' });
      await waitPublishing(socket, true, 'camera_on (before switch)');
      await sleep(options.settleMs);
      // Which device is live is not readable here, so switch to the second
      // and then the first: at least one of the two is a real switch (the
      // other is the #842 no-op), and the preference ends on the first.
      step('switching camera device (twice, so at least one is a real switch)');
      for (const device of [devices[1], devices[0]]) {
        const applied = await socket.ok({ cmd: 'set_camera_device', device_id: device.id });
        if (!applied.applied) warn(`switch not applied${applied.error ? ' (see log)' : ''}`);
        await waitPublishing(socket, true, 'set_camera_device');
        await sleep(options.settleMs);
      }
      findings.deviceSwitch = 'done';
      step('meeting camera OFF');
      await socket.ok({ cmd: 'camera_off' });
      await waitPublishing(socket, false, 'camera_off (after switch)');
      await sleep(options.settleMs);
    }

    // 7. Tidy up what this script opened.
    if (!options.keepSettings) {
      await socket.ok({ cmd: 'close_settings' });
      await tail.waitFor(PREVIEW_RELEASED, 5_000);
    }
    if (findings.joinedHere) await socket.ok({ cmd: 'leave' });
  } finally {
    socket.close();
  }
  return findings;
}

function logOffsetOfNow(path) {
  // Where this run sits in the analyzer's own offset-from-first-line clock,
  // so its episodes can be told apart from older ones in the same file.
  const fd = openSync(path, 'r');
  const head = Buffer.alloc(64);
  try {
    readSync(fd, head, 0, 64, 0);
  } finally {
    closeSync(fd);
  }
  const first = Date.parse(`${head.toString('utf8').slice(0, 23).replace(' ', 'T')}Z`);
  if (!Number.isFinite(first)) return null;
  const delta = Math.max(0, Date.now() - first);
  const s = Math.floor(delta / 1000);
  const pad = (n) => String(n).padStart(2, '0');
  return `+${pad(Math.floor(s / 3600))}:${pad(Math.floor((s % 3600) / 60))}:${pad(s % 60)}`;
}

async function main(argv) {
  let options;
  try {
    options = parseArgs(argv);
  } catch (error) {
    console.error(`measure-camera-intent: ${error.message}\n\n${USAGE}`);
    return 2;
  }
  if (options.help) {
    console.log(USAGE);
    return 0;
  }
  if (!options.socket) {
    console.error(`measure-camera-intent: no socket. Pass --socket or set PETAL_AUTOTEST_SOCK.\n\n${USAGE}`);
    return 2;
  }
  try {
    options.log = options.log ? resolve(options.log) : newestPetalLog();
  } catch (error) {
    console.error(`measure-camera-intent: ${error.message}`);
    return 2;
  }

  const startedAt = logOffsetOfNow(options.log);
  console.log(`# #76 runbook over ${options.socket}`);
  let findings;
  try {
    findings = await runbook(options);
  } catch (error) {
    console.error(`not ok: ${error.message}`);
    return 1;
  }
  console.log('');
  console.log(`preview live before the first toggle: ${findings.previewLive ? 'yes' : 'NO'}`);
  console.log(`device switch: ${findings.deviceSwitch}`);
  if (startedAt) console.log(`this run's episodes start at about ${startedAt} in the report below`);
  console.log('');

  if (!options.analyze) return 0;
  const args = [ANALYZER, ...(options.json ? ['--json'] : []), options.log];
  const result = spawnSync(process.execPath, args, { stdio: 'inherit' });
  return result.status ?? 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main(process.argv.slice(2)).then((code) => {
    process.exitCode = code;
  });
}

export { parseArgs, newestPetalLog };
