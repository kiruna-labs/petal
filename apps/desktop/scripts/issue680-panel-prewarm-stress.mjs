#!/usr/bin/env node

import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import process from 'node:process';

const DEFAULT_ITERATIONS = 25;
const DEFAULT_COMMAND_TIMEOUT_MS = 30_000;
const DEFAULT_LIVENESS_TIMEOUT_MS = 10_000;

function usage(message) {
  if (message) console.error(message);
  console.error(`Usage:
  node scripts/issue680-panel-prewarm-stress.mjs --socket <path> --list-windows
  node scripts/issue680-panel-prewarm-stress.mjs --socket <path> \\
    --window-id <id> --window-id <id> [--iterations <n>] [--log <path>]

The Petal process must already be joined to a room with PETAL_AUTOTEST_SOCK
enabled. The two IDs must name sacrificial, on-screen windows.`);
  process.exit(2);
}

function parsePositiveInteger(raw, name) {
  const value = Number(raw);
  if (!Number.isSafeInteger(value) || value <= 0) usage(`${name} must be a positive integer`);
  return value;
}

function parseArgs(argv) {
  const options = {
    socket: process.env.PETAL_AUTOTEST_SOCK,
    log: path.join(os.homedir(), 'Library', 'Logs', 'Petal', 'petal.log'),
    iterations: DEFAULT_ITERATIONS,
    windowIds: [],
    listWindows: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    switch (arg) {
      case '--socket':
        options.socket = argv[++index];
        break;
      case '--log':
        options.log = argv[++index];
        break;
      case '--iterations':
        options.iterations = parsePositiveInteger(argv[++index], '--iterations');
        break;
      case '--window-id':
        options.windowIds.push(parsePositiveInteger(argv[++index], '--window-id'));
        break;
      case '--list-windows':
        options.listWindows = true;
        break;
      case '--help':
      case '-h':
        usage();
        break;
      default:
        usage(`unknown argument: ${arg}`);
    }
  }
  if (!options.socket) usage('--socket (or PETAL_AUTOTEST_SOCK) is required');
  if (!options.listWindows) {
    if (options.windowIds.length !== 2 || options.windowIds[0] === options.windowIds[1]) {
      usage('provide exactly two distinct --window-id values');
    }
    if (!fs.existsSync(options.log)) usage(`Petal log not found: ${options.log}`);
  }
  return options;
}

function connect(socketPath) {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(socketPath);
    socket.once('connect', () => resolve(socket));
    socket.once('error', reject);
  });
}

function commandClient(socket) {
  socket.setEncoding('utf8');
  let buffer = '';
  let pending;

  socket.on('data', (chunk) => {
    buffer += chunk;
    let newline;
    while ((newline = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, newline);
      buffer = buffer.slice(newline + 1);
      if (!line.trim()) continue;
      const request = pending;
      pending = undefined;
      if (!request) continue;
      clearTimeout(request.timer);
      try {
        request.resolve(JSON.parse(line));
      } catch (error) {
        request.reject(error);
      }
    }
  });
  socket.on('error', (error) => {
    if (!pending) return;
    clearTimeout(pending.timer);
    pending.reject(error);
    pending = undefined;
  });

  return (command, timeoutMs = DEFAULT_COMMAND_TIMEOUT_MS) =>
    new Promise((resolve, reject) => {
      if (pending) return reject(new Error('autotest command already pending'));
      const timer = setTimeout(() => {
        pending = undefined;
        reject(new Error(`autotest command timed out after ${timeoutMs}ms: ${command.cmd}`));
      }, timeoutMs);
      pending = { resolve, reject, timer };
      socket.write(`${JSON.stringify(command)}\n`);
    });
}

async function requireOk(send, command) {
  const response = await send(command);
  if (!response.ok) throw new Error(`${command.cmd} failed: ${response.error}`);
  return response.result;
}

function assertSharedState(state, windowIds, expected) {
  const sessionIds = new Set(state.sessionSharedWindowIds ?? []);
  const hoverIds = new Set(state.hoverSharedWindowIds ?? []);
  for (const windowId of windowIds) {
    if (sessionIds.has(windowId) !== expected || hoverIds.has(windowId) !== expected) {
      throw new Error(
        `window ${windowId} shared=${expected} assertion failed: ` +
          `session=${JSON.stringify([...sessionIds])}, hover=${JSON.stringify([...hoverIds])}`
      );
    }
  }
}

function readAppended(logPath, state) {
  const size = fs.statSync(logPath).size;
  if (size < state.offset) {
    state.offset = 0;
    state.partial = '';
  }
  if (size === state.offset) return '';
  const length = size - state.offset;
  const fd = fs.openSync(logPath, 'r');
  try {
    const bytes = Buffer.alloc(length);
    fs.readSync(fd, bytes, 0, length, state.offset);
    state.offset = size;
    return bytes.toString('utf8');
  } finally {
    fs.closeSync(fd);
  }
}

function livenessMarkers(text) {
  const markers = [];
  const patterns = [
    // Current main (#677): unconditional main-thread sampling successor to
    // the original ~400ms observation used in issue #680's incident proof.
    /hover_tab: \[focus\] measure summary window (\d+) generation (\d+)/g,
    // Pre-#677 builds retain the exact marker named in issue #680.
    /hover_tab: \[focus\] selection handback observation window (\d+) generation (\d+) after ~400ms/g,
  ];
  for (const pattern of patterns) {
    for (const match of text.matchAll(pattern)) {
      markers.push({ windowId: Number(match[1]), generation: Number(match[2]) });
    }
  }
  return markers;
}

async function waitForGenerationMarkers(logPath, logState, windowIds) {
  const deadline = Date.now() + DEFAULT_LIVENESS_TIMEOUT_MS;
  const found = new Map();
  while (Date.now() <= deadline) {
    const appended = readAppended(logPath, logState);
    const complete = logState.partial + appended;
    const lines = complete.split('\n');
    logState.partial = lines.pop() ?? '';
    for (const marker of livenessMarkers(lines.join('\n'))) {
      if (windowIds.includes(marker.windowId)) found.set(marker.windowId, marker.generation);
    }
    if (windowIds.every((windowId) => found.has(windowId))) return found;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(
    `main-thread liveness marker missing after ${DEFAULT_LIVENESS_TIMEOUT_MS}ms; ` +
      `observed=${JSON.stringify(Object.fromEntries(found))}, expected windows=${windowIds.join(',')}`
  );
}

const options = parseArgs(process.argv.slice(2));
const socket = await connect(options.socket);
const send = commandClient(socket);

try {
  if (options.listWindows) {
    const result = await requireOk(send, { cmd: 'list_windows' });
    console.log(JSON.stringify(result, null, 2));
    process.exitCode = 0;
  } else {
    const initial = await requireOk(send, { cmd: 'dump_state' });
    if (!initial.currentRoom) throw new Error('Petal is not joined to a room');

    const logState = { offset: fs.statSync(options.log).size, partial: '' };
    const generations = new Set();
    for (const windowId of options.windowIds) {
      await requireOk(send, { cmd: 'stop_share', window_id: windowId });
    }

    for (let iteration = 1; iteration <= options.iterations; iteration += 1) {
      await requireOk(send, { cmd: 'share', window_id: options.windowIds[0] });
      await requireOk(send, { cmd: 'share', window_id: options.windowIds[1] });

      const shared = await requireOk(send, { cmd: 'dump_state' });
      assertSharedState(shared, options.windowIds, true);

      const observed = await waitForGenerationMarkers(options.log, logState, options.windowIds);
      for (const [windowId, generation] of observed) {
        const key = `${windowId}:${generation}`;
        if (generations.has(key)) throw new Error(`duplicate liveness marker ${key}`);
        generations.add(key);
      }

      await requireOk(send, { cmd: 'stop_share', window_id: options.windowIds[1] });
      await requireOk(send, { cmd: 'stop_share', window_id: options.windowIds[0] });
      const stopped = await requireOk(send, { cmd: 'dump_state' });
      assertSharedState(stopped, options.windowIds, false);

      console.log(
        `ok iteration ${iteration}/${options.iterations}: ` +
          options.windowIds.map((id) => `window ${id} generation ${observed.get(id)}`).join(', ')
      );
    }

    const expected = options.iterations * options.windowIds.length;
    if (generations.size !== expected) {
      throw new Error(`expected ${expected} unique generation markers, observed ${generations.size}`);
    }
    console.log(
      `PASS: ${options.iterations} two-window share/unshare iterations; ` +
        `${generations.size}/${expected} generations reported main-thread liveness`
    );
  }
} catch (error) {
  console.error(`FAIL: ${error.message}`);
  process.exitCode = 1;
} finally {
  socket.end();
}
