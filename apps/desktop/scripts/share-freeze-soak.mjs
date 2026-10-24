#!/usr/bin/env node
// Share-freeze soak (#391): a native window share held open on a REAL LiveKit
// deployment (tokens from the production or staged backend) for minutes, with
// two independent web viewers measuring every presented frame.
//
//   node apps/desktop/scripts/share-freeze-soak.mjs --socket <petal autotest socket>
//     --harness-url https://meet.petal.live --duration-ms 600000 --out <dir>
//
// The running Petal must already be up with PETAL_AUTOTEST_SOCK and
// PETAL_BACKEND_URL set (nightly-loopback.yml's share-freeze-soak job). This
// script joins it to a fresh room, shares an animated fixture window
// (share-freeze-animator.swift), starts N headless Chrome viewers (separate
// processes and profiles, so one browser's hiccup cannot fake a correlated
// freeze), injects the measurement over CDP (so it does not depend on which
// web build is deployed), and writes <out>/share-freeze-raw.json. It judges
// nothing: scripts/ci/share-freeze-verdict.mjs does, from that file.
//
// It always writes the raw file, even on an early failure: `infra` lists what
// stopped the run, and the verdict scores that INFRA-INVALID, never a pass.
import { spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

import { connectSocket } from './autotest-socket.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 1) {
    const key = argv[i];
    if (!key.startsWith('--')) throw new Error(`unexpected argument '${key}'`);
    args[key.slice(2)] = argv[i + 1] ?? '';
    i += 1;
  }
  return args;
}

const args = parseArgs(process.argv.slice(2));
const socketPath = args.socket || process.env.PETAL_AUTOTEST_SOCK;
const harnessUrl = (args['harness-url'] || process.env.PETAL_HARNESS_URL || 'https://meet.petal.live').replace(/\/+$/, '');
const durationMs = Number(args['duration-ms'] || 600_000);
const viewerCount = Number(args.viewers || 2);
const outDir = args.out || path.join(os.tmpdir(), `petal-share-freeze-${Date.now()}`);
const cdpBasePort = Number(args['cdp-base-port'] || 9333);
const warmupMs = Number(args['warmup-ms'] || 10_000);
const chromeBin = process.env.PETAL_CHROME_BIN || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
// Never logged or written: only Chrome's first navigation carries it (#42).
const bypassSecret = (process.env.PETAL_VERCEL_BYPASS_SECRET || '').trim();

// Keep in lockstep with share-freeze-animator.swift.
const SOURCE_WIDTH = 960;
const SOURCE_HEIGHT = 600;
const STRIP_X = 84;
const STRIP_BLOCK = 44;
const STRIP_BLOCKS = 18;
const STRIP_SAMPLE_Y = 290;
const STRIP_SAMPLE_H = 20;

const animatorSource = path.join(HERE, 'share-freeze-animator.swift');
const animatorBundle = path.join(os.tmpdir(), 'PetalFreezeAnimator.app');
const animatorBinary = path.join(animatorBundle, 'Contents', 'MacOS', 'PetalFreezeAnimator');

fs.mkdirSync(outDir, { recursive: true });
const ticksLog = path.join(outDir, 'animator-ticks.log');

function hostInfo() {
  const run = (cmd, argv) => {
    const result = spawnSync(cmd, argv, { encoding: 'utf8' });
    return result.status === 0 ? result.stdout.trim() : null;
  };
  return {
    macos: run('sw_vers', ['-productVersion']),
    build: run('sw_vers', ['-buildVersion']),
    arch: process.arch,
    cpus: os.cpus().length,
    memGiB: Math.round(os.totalmem() / 2 ** 30),
    chrome: run(chromeBin, ['--version']),
  };
}

const raw = {
  schema: 1,
  harnessUrl,
  requestedDurationMs: durationMs,
  warmupMs,
  viewerCount,
  testSharePause: process.env.PETAL_TEST_SHARE_PAUSE || null,
  startedAtMs: Date.now(),
  host: hostInfo(),
  measureStartMs: null,
  measureEndMs: null,
  endedAtMs: null,
  sharer: null,
  viewers: [],
  sender: [],
  fixture: { ticksLog: path.basename(ticksLog), ticks: [] },
  infra: [],
  fatal: [],
  events: [],
};

const children = [];
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const note = (message) => {
  raw.events.push([Date.now(), message]);
  console.log(`# ${message}`);
};

async function waitUntil(label, fn, timeoutMs, intervalMs = 250) {
  const deadline = Date.now() + timeoutMs;
  let last;
  while (Date.now() <= deadline) {
    last = await fn();
    if (last) return last;
    await sleep(intervalMs);
  }
  throw new Error(`${label} timed out after ${timeoutMs}ms; last=${JSON.stringify(last)}`);
}

async function command(client, payload) {
  const response = await client.send(payload);
  if (!response.ok) throw new Error(`${payload.cmd} failed: ${response.error}`);
  return response.result;
}

function compileAnimator() {
  fs.mkdirSync(path.dirname(animatorBinary), { recursive: true });
  fs.writeFileSync(
    path.join(animatorBundle, 'Contents', 'Info.plist'),
    `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>PetalFreezeAnimator</string>
<key>CFBundleIdentifier</key><string>com.petal.testing.freeze-animator</string>
<key>CFBundleName</key><string>PetalFreezeAnimator</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>1.0</string>
<key>CFBundleVersion</key><string>1</string>
</dict></plist>
`
  );
  const result = spawnSync('xcrun', ['swiftc', animatorSource, '-framework', 'AppKit', '-o', animatorBinary], {
    encoding: 'utf8',
    env: {
      ...process.env,
      SWIFT_MODULE_CACHE_PATH: path.join(os.tmpdir(), 'petal-freeze-swift-cache'),
      CLANG_MODULE_CACHE_PATH: path.join(os.tmpdir(), 'petal-freeze-clang-cache'),
    },
  });
  if (result.status !== 0) {
    throw new Error(`could not compile the freeze animator: ${(result.stderr || result.stdout || result.error?.message || '').trim()}`);
  }
}

async function startAnimator() {
  compileAnimator();
  const child = spawn(animatorBinary, [], {
    detached: true,
    stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, PETAL_FREEZE_ANIMATOR_LOG: ticksLog },
  });
  children.push({ name: 'animator', child });
  let stdout = '';
  let stderr = '';
  child.stdout.setEncoding('utf8').on('data', (chunk) => { stdout += chunk; });
  child.stderr.setEncoding('utf8').on('data', (chunk) => { stderr += chunk; });
  await waitUntil('freeze animator ready', () => {
    if (child.exitCode !== null) throw new Error(`freeze animator exited ${child.exitCode}: ${stderr.trim()}`);
    return stdout.includes('PETAL_FREEZE_ANIMATOR_READY');
  }, 20_000, 100);
  return child;
}

function chromeCommand() {
  // Same reason as the cockpit's chrome_command: on Apple Silicon a plain
  // launch can pick Chrome's x86_64 slice under Rosetta and throttle badly.
  return process.arch === 'arm64' ? ['arch', ['-arm64', chromeBin]] : [chromeBin, []];
}

async function startViewerBrowser(index) {
  const port = cdpBasePort + index;
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), `petal-freeze-viewer-${index}-`));
  const log = fs.openSync(path.join(outDir, `chrome-viewer-${index}.log`), 'w');
  const [bin, prefix] = chromeCommand();
  const child = spawn(bin, [
    ...prefix,
    '--headless=new',
    '--use-mock-keychain',
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-renderer-backgrounding',
    '--disable-background-timer-throttling',
    '--disable-backgrounding-occluded-windows',
    '--autoplay-policy=no-user-gesture-required',
    '--window-size=1280,800',
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${profile}`,
    'about:blank',
  ], { stdio: ['ignore', log, log] });
  children.push({ name: `chrome-viewer-${index}`, child, profile });
  const page = await waitUntil(`chrome viewer ${index} CDP on :${port}`, async () => {
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
      return list.find((target) => target.type === 'page' && target.webSocketDebuggerUrl) ?? null;
    } catch {
      return null;
    }
  }, 20_000, 250);
  return { port, child, wsUrl: page.webSocketDebuggerUrl };
}

function connectCdp(wsUrl) {
  const ws = new WebSocket(wsUrl);
  let nextId = 1;
  const pending = new Map();
  let closed = null;
  ws.addEventListener('message', (event) => {
    const message = JSON.parse(event.data);
    if (!message.id) return;
    const waiter = pending.get(message.id);
    if (!waiter) return;
    pending.delete(message.id);
    if (message.error) waiter.reject(new Error(message.error.message));
    else waiter.resolve(message.result);
  });
  ws.addEventListener('close', () => {
    closed = 'CDP websocket closed';
    for (const waiter of pending.values()) waiter.reject(new Error(closed));
    pending.clear();
  });
  return new Promise((resolve, reject) => {
    ws.addEventListener('open', () => resolve({
      call(method, params = {}) {
        if (closed) return Promise.reject(new Error(closed));
        const id = nextId++;
        ws.send(JSON.stringify({ id, method, params }));
        return new Promise((callResolve, callReject) => {
          pending.set(id, { resolve: callResolve, reject: callReject });
          setTimeout(() => {
            if (pending.delete(id)) callReject(new Error(`CDP ${method} timed out`));
          }, 30_000).unref?.();
        });
      },
      close() { ws.close(); },
    }));
    ws.addEventListener('error', () => reject(new Error(`could not connect CDP ${wsUrl}`)));
  });
}

async function evaluate(cdp, expression) {
  const result = await cdp.call('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
  if (result.exceptionDetails) {
    throw new Error(result.exceptionDetails.exception?.description || result.exceptionDetails.text || 'evaluation failed');
  }
  return result.result?.value;
}

async function joinViewer(cdp, accessCode, label) {
  const url = `${harnessUrl}/?code=${encodeURIComponent(accessCode)}`;
  const navigation = bypassSecret
    ? `${url}&x-vercel-protection-bypass=${encodeURIComponent(bypassSecret)}&x-vercel-set-bypass-cookie=true`
    : url;
  await cdp.call('Page.enable');
  await cdp.call('Page.navigate', { url: navigation });
  await waitUntil(`${label} harness DOM`, () => evaluate(cdp,
    `!!document.querySelector('#display-name') && !!document.querySelector('#meeting-code') && !!document.querySelector('#join-btn') && !!window.__petalHarness`
  ).catch(() => false), 30_000);
  await evaluate(cdp, `(() => {
    const display = document.querySelector('#display-name');
    const code = document.querySelector('#meeting-code');
    display.value = ${JSON.stringify(label)};
    code.value = ${JSON.stringify(accessCode)};
    display.dispatchEvent(new Event('input', { bubbles: true }));
    code.dispatchEvent(new Event('input', { bubbles: true }));
    if (window.__petalHarness?.room?.state !== 'connected') document.querySelector('#join-btn').click();
    return true;
  })()`);
  return waitUntil(`${label} connected`, () => evaluate(cdp, `(() => {
    const room = window.__petalHarness?.room;
    return room?.state === 'connected' ? { name: room.name ?? null, identity: room.localParticipant?.identity ?? null } : null;
  })()`).catch(() => null), 45_000, 250);
}

// Runs inside each viewer page. Presented-frame timeline (rVFC) with the
// decoded source tick, inbound-rtp stats every 2s, and an event-loop lag probe
// so a stalled renderer is not mistaken for a stalled stream.
function measurementScript(owner) {
  return `(() => {
  if (window.__freezeSoak) return 'already-installed';
  const OWNER = ${JSON.stringify(owner)};
  const S = { frames: [], stats: [], lag: [], events: [], video: null, armed: 0 };
  const origin = performance.timeOrigin;
  const canvas = document.createElement('canvas');
  canvas.width = ${STRIP_BLOCKS}; canvas.height = 1;
  const g = canvas.getContext('2d', { willReadFrequently: true });
  const sx = ${STRIP_X / SOURCE_WIDTH}, sw = ${(STRIP_BLOCK * STRIP_BLOCKS) / SOURCE_WIDTH};
  const sy = ${STRIP_SAMPLE_Y / SOURCE_HEIGHT}, sh = ${STRIP_SAMPLE_H / SOURCE_HEIGHT};
  function decode(video) {
    const vw = video.videoWidth, vh = video.videoHeight;
    if (!vw || !vh) return null;
    try {
      g.drawImage(video, sx * vw, sy * vh, sw * vw, sh * vh, 0, 0, ${STRIP_BLOCKS}, 1);
      const d = g.getImageData(0, 0, ${STRIP_BLOCKS}, 1).data;
      const bits = [];
      for (let i = 0; i < ${STRIP_BLOCKS}; i++) {
        const o = i * 4;
        bits.push(0.299 * d[o] + 0.587 * d[o + 1] + 0.114 * d[o + 2] > 128 ? 1 : 0);
      }
      if (bits[0] !== 1 || bits[${STRIP_BLOCKS - 1}] !== 0) return null;
      let gray = 0;
      for (let i = 1; i < ${STRIP_BLOCKS - 1}; i++) gray = (gray << 1) | bits[i];
      let bin = gray;
      for (let s = gray >> 1; s; s >>= 1) bin ^= s;
      return bin;
    } catch { return null; }
  }
  function arm(video) {
    const id = ++S.armed;
    const onFrame = (now, meta) => {
      if (id !== S.armed) return;
      const t = origin + (meta?.expectedDisplayTime ?? now);
      S.frames.push([Math.round(t * 10) / 10, decode(video), meta?.rtpTimestamp ?? null,
        video.videoWidth, video.videoHeight, meta?.presentedFrames ?? null]);
      video.requestVideoFrameCallback(onFrame);
    };
    video.requestVideoFrameCallback(onFrame);
  }
  const findVideo = () => document.querySelector('.share-tile[data-owner="' + CSS.escape(OWNER) + '"] video');
  setInterval(() => {
    const v = findVideo();
    if (v !== S.video) {
      S.events.push([Date.now(), v ? 'video-element-bound' : 'video-element-gone']);
      S.video = v;
      if (v) arm(v);
    }
  }, 250);
  const FIELDS = ['framesReceived', 'framesDecoded', 'framesDropped', 'framesPerSecond', 'frameWidth', 'frameHeight',
    'freezeCount', 'totalFreezesDuration', 'pauseCount', 'totalPausesDuration', 'jitterBufferDelay',
    'jitterBufferTargetDelay', 'jitterBufferEmittedCount', 'packetsReceived', 'packetsLost', 'nackCount', 'pliCount',
    'firCount', 'keyFramesDecoded', 'totalDecodeTime', 'totalInterFrameDelay', 'decoderImplementation', 'bytesReceived'];
  async function sample() {
    const room = window.__petalHarness?.room;
    const participant = room?.remoteParticipants?.get(OWNER);
    let pub = null;
    if (participant) for (const p of participant.trackPublications.values()) {
      if (String(p.trackName || '').startsWith('petal-window-') && p.track) pub = p;
    }
    const row = { t: Date.now(), roomState: room?.state ?? null, subscribed: !!pub, trackName: pub?.trackName ?? null };
    if (pub?.track?.getRTCStatsReport) {
      const report = await pub.track.getRTCStatsReport();
      report?.forEach((s) => {
        if (s.type === 'inbound-rtp' && s.kind === 'video') for (const f of FIELDS) if (s[f] !== undefined) row[f] = s[f];
        if (s.type === 'candidate-pair' && s.nominated && s.currentRoundTripTime !== undefined) row.rttMs = s.currentRoundTripTime * 1000;
      });
    }
    S.stats.push(row);
  }
  setInterval(() => { sample().catch((e) => S.events.push([Date.now(), 'stats-error ' + e])); }, 2000);
  let last = performance.now();
  setInterval(() => {
    const now = performance.now();
    const lag = now - last - 50;
    if (lag > 200) S.lag.push([Math.round(origin + last), Math.round(lag)]);
    last = now;
  }, 50);
  S.drain = () => {
    const out = { frames: S.frames, stats: S.stats, lag: S.lag, events: S.events };
    S.frames = []; S.stats = []; S.lag = []; S.events = [];
    return out;
  };
  window.__freezeSoak = S;
  return 'installed';
})()`;
}

async function drainViewer(viewer) {
  if (!viewer.cdp || viewer.dead) return;
  try {
    const batch = await evaluate(viewer.cdp, 'window.__freezeSoak ? window.__freezeSoak.drain() : null');
    if (!batch) throw new Error('measurement hook missing (page reloaded?)');
    viewer.record.frames.push(...batch.frames);
    viewer.record.stats.push(...batch.stats);
    viewer.record.lag.push(...batch.lag);
    viewer.record.events.push(...batch.events);
  } catch (error) {
    viewer.dead = true;
    raw.infra.push(`${viewer.record.label}: lost the measurement page mid-run: ${error.message}`);
  }
}

function senderRow(metrics) {
  const tracks = metrics?.network?.tracks ?? [];
  const track = tracks.find((t) => t.direction === 'send' && String(t.name || '').startsWith('petal-window-'));
  if (!track) return { t: Date.now(), track: null };
  const pick = ['name', 'width', 'height', 'fps', 'codec_impl', 'quality_limitation', 'software_encoder', 'target_kbps',
    'actual_kbps', 'packets_lost', 'frames_encoded', 'key_frames_encoded', 'nack_count', 'fir_count', 'pli_count'];
  const row = { t: Date.now(), track: true };
  for (const key of pick) {
    const camel = key.replace(/_([a-z])/g, (_, c) => c.toUpperCase());
    if (track[key] !== undefined) row[key] = track[key];
    else if (track[camel] !== undefined) row[key] = track[camel];
  }
  return row;
}

function readTicks() {
  try {
    return fs.readFileSync(ticksLog, 'utf8').split('\n').filter(Boolean).map((line) => {
      const [tick, ms] = line.split(' ').map(Number);
      return [tick, ms];
    }).filter(([tick, ms]) => Number.isFinite(tick) && Number.isFinite(ms));
  } catch {
    return [];
  }
}

async function stopChild({ name, child, profile }) {
  if (child.exitCode === null && child.signalCode === null) {
    try { child.kill('SIGTERM'); } catch { /* already gone */ }
    const deadline = Date.now() + 5000;
    while (child.exitCode === null && child.signalCode === null && Date.now() < deadline) await sleep(100);
    if (child.exitCode === null && child.signalCode === null) {
      try { child.kill('SIGKILL'); } catch { /* already gone */ }
      await sleep(300);
    }
  }
  const alive = child.exitCode === null && child.signalCode === null;
  if (alive) console.log(`::warning::share-freeze soak: ${name} (pid ${child.pid}) is still alive after SIGKILL`);
  if (profile) fs.rmSync(profile, { recursive: true, force: true });
  return !alive;
}

async function main() {
  if (!socketPath) throw new Error('no --socket / PETAL_AUTOTEST_SOCK');
  if (!Number.isFinite(durationMs) || durationMs < 30_000) throw new Error(`--duration-ms must be >= 30000 (got ${durationMs})`);
  if (!Number.isInteger(viewerCount) || viewerCount < 2) throw new Error(`--viewers must be >= 2 (got ${viewerCount})`);
  if (!fs.existsSync(chromeBin)) throw new Error(`Chrome not found at ${chromeBin}`);

  const client = connectSocket(socketPath);
  const suffix = crypto.randomBytes(4).toString('hex');
  const identity = `p-freezesoak-${suffix}`;
  let joined;
  try {
    joined = await command(client, { cmd: 'join', room: `freezesoak-${suffix}`, identity, name: 'Freeze Soak Sharer' });
  } catch (error) {
    // A token mint or SFU connect failure lands here: the run measured nothing.
    raw.infra.push(`native join to a fresh room failed (token mint / SFU connect): ${error.message}`);
    return;
  }
  const room = await command(client, { cmd: 'current_room' });
  if (!room?.accessCode) {
    raw.infra.push(`joined room exposes no access code (${JSON.stringify(room)}), so no viewer can join it`);
    return;
  }
  raw.sharer = { identity, livekitRoom: room.livekitRoom, joinedRoomId: joined?.id ?? null };
  note(`native sharer joined livekitRoom=${room.livekitRoom} as ${identity}`);

  let animator;
  try {
    animator = await startAnimator();
  } catch (error) {
    raw.infra.push(`fixture: ${error.message}`);
    return;
  }
  let shared;
  try {
    shared = await waitUntil('share the animator window', async () => {
      const response = await client.send({ cmd: 'share_matching', pid: animator.pid });
      return response.ok ? response.result : null;
    }, 20_000, 500);
  } catch (error) {
    raw.infra.push(`could not share the animator window: ${error.message}`);
    return;
  }
  raw.sharer.windowId = shared.windowId;
  note(`sharing animator window ${shared.windowId}`);

  const viewers = [];
  for (let i = 0; i < viewerCount; i += 1) {
    const label = `freeze-viewer-${i + 1}`;
    const record = { label, identity: null, joinedAtMs: null, firstFrameAtMs: null, frames: [], stats: [], lag: [], events: [] };
    raw.viewers.push(record);
    const viewer = { record, cdp: null, dead: false };
    viewers.push(viewer);
    try {
      const browser = await startViewerBrowser(i);
      viewer.cdp = await connectCdp(browser.wsUrl);
      const connected = await joinViewer(viewer.cdp, room.accessCode, label);
      if (connected.name && room.livekitRoom && connected.name !== room.livekitRoom) {
        throw new Error(`joined LiveKit room ${connected.name}, sharer is in ${room.livekitRoom}`);
      }
      record.identity = connected.identity;
      record.joinedAtMs = Date.now();
      await evaluate(viewer.cdp, measurementScript(identity));
      note(`${label} joined as ${connected.identity} and is measuring`);
    } catch (error) {
      viewer.dead = true;
      raw.infra.push(`${label}: could not join the room or install the measurement: ${error.message}`);
    }
  }
  if (viewers.some((v) => v.dead)) return;

  // Every viewer must present a first frame before the clock starts; one that
  // never does is a rig failure (nothing measured), not a freeze.
  try {
    await waitUntil('every viewer presents a first frame', async () => {
      for (const viewer of viewers) {
        await drainViewer(viewer);
        if (!viewer.record.firstFrameAtMs && viewer.record.frames.length) viewer.record.firstFrameAtMs = viewer.record.frames[0][0];
      }
      return viewers.every((v) => v.record.firstFrameAtMs);
    }, 90_000, 1000);
  } catch (error) {
    const without = viewers.filter((v) => !v.record.firstFrameAtMs).map((v) => v.record.label);
    raw.infra.push(`no presented frame on ${without.join(', ')} within 90s of joining: ${error.message}`);
    return;
  }
  await sleep(warmupMs);
  raw.measureStartMs = Date.now();
  const end = raw.measureStartMs + durationMs;
  note(`measuring for ${durationMs}ms (until ${new Date(end).toISOString()})`);

  let nextSender = 0;
  let nextDrain = 0;
  let nextProgress = Date.now() + 60_000;
  while (Date.now() < end) {
    const now = Date.now();
    if (now >= nextSender) {
      nextSender = now + 2000;
      try {
        raw.sender.push(senderRow(await command(client, { cmd: 'dump_metrics', journal_tail: 0 })));
      } catch (error) {
        // The sharer stopped answering: that is a product failure, not a rig
        // one (the job's crash-report check says whether it died).
        raw.fatal.push(`sharer stopped answering the autotest socket at ${new Date(now).toISOString()}: ${error.message}`);
        break;
      }
    }
    if (now >= nextDrain) {
      nextDrain = now + 5000;
      for (const viewer of viewers) await drainViewer(viewer);
      if (viewers.some((v) => v.dead)) break;
      if (animator.exitCode !== null || animator.signalCode !== null) {
        raw.infra.push(`the animator fixture exited mid-run (${animator.exitCode ?? animator.signalCode})`);
        break;
      }
    }
    if (now >= nextProgress) {
      nextProgress = now + 60_000;
      const counts = viewers.map((v) => `${v.record.label}=${v.record.frames.length}`).join(' ');
      note(`progress ${Math.round((now - raw.measureStartMs) / 1000)}s frames ${counts}`);
    }
    await sleep(250);
  }
  raw.measureEndMs = Math.min(Date.now(), end);
  for (const viewer of viewers) await drainViewer(viewer);
  try {
    await command(client, { cmd: 'stop_share', window_id: shared.windowId });
    await command(client, { cmd: 'leave' });
  } catch (error) {
    note(`teardown: ${error.message}`);
  }
  client.close();
  for (const viewer of viewers) viewer.cdp?.close();
}

let exitCode = 0;
try {
  await main();
} catch (error) {
  raw.infra.push(`harness error: ${error?.stack ?? error}`);
} finally {
  raw.endedAtMs = Date.now();
  raw.fixture.ticks = readTicks();
  for (const entry of children.reverse()) await stopChild(entry);
  const file = path.join(outDir, 'share-freeze-raw.json');
  fs.writeFileSync(file, `${JSON.stringify(raw)}\n`);
  console.log(`share-freeze raw evidence: ${file} (infra=${raw.infra.length} fatal=${raw.fatal.length})`);
  for (const problem of raw.infra) console.log(`::warning::share-freeze soak infra: ${problem}`);
  for (const problem of raw.fatal) console.log(`::error::share-freeze soak: ${problem}`);
  if (raw.infra.length || raw.fatal.length) exitCode = 2;
}
process.exitCode = exitCode;
