#!/usr/bin/env node
// Camera-freeze soak (#456): a native CAMERA publish held open on a REAL
// LiveKit deployment for minutes, with two independent headless-Chrome
// viewers recording every presented camera frame and the inbound-rtp stats.
//
//   node apps/desktop/scripts/camera-freeze-soak.mjs --socket <petal autotest socket>
//     --harness-url https://meet.petal.live --duration-ms 300000 --out <dir> [--with-share 1]
//
// The running Petal must already be up with PETAL_AUTOTEST_SOCK,
// PETAL_BACKEND_URL and PETAL_CAMERA_SYNTH_SOURCE=1 (+ PETAL_CAMERA_SYNTH_REALISTIC=1
// on a debug build for a camera-sized, camera-cost picture). This script joins it
// to a fresh room, turns the camera on (optionally also shares the
// share-freeze animator window, so camera and share contend for the uplink as
// in a real meeting), and writes <out>/camera-freeze-raw.json. It judges
// nothing: scripts/ci/camera-freeze-summary.mjs scores that file.
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
const durationMs = Number(args['duration-ms'] || 300_000);
const viewerCount = Number(args.viewers || 2);
const withShare = args['with-share'] === '1' || args['with-share'] === 'true';
// Repaint the shared window only every N ms (a mostly static share, as in a
// code review); 0 keeps the 30 fps animation.
const shareStaticMs = Number(args['share-static-ms'] || 0);
const outDir = args.out || path.join(os.tmpdir(), `petal-camera-freeze-${Date.now()}`);
const cdpBasePort = Number(args['cdp-base-port'] || 9353);
const warmupMs = Number(args['warmup-ms'] || 5_000);
const chromeBin = process.env.PETAL_CHROME_BIN || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const bypassSecret = (process.env.PETAL_VERCEL_BYPASS_SECRET || '').trim();

fs.mkdirSync(outDir, { recursive: true });

const raw = {
  schema: 1,
  kind: 'camera-freeze-soak',
  harnessUrl,
  requestedDurationMs: durationMs,
  warmupMs,
  viewerCount,
  withShare,
  shareStaticMs,
  startedAtMs: Date.now(),
  host: {
    macos: spawnSync('sw_vers', ['-productVersion'], { encoding: 'utf8' }).stdout?.trim() ?? null,
    arch: process.arch,
    cpus: os.cpus().length,
  },
  cameraPublishedAtMs: null,
  measureStartMs: null,
  measureEndMs: null,
  endedAtMs: null,
  sharer: null,
  viewers: [],
  sender: [],
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

// Optional share load: the same animated fixture the share-freeze soak uses.
async function startAnimator() {
  const source = path.join(HERE, 'share-freeze-animator.swift');
  const bundle = path.join(os.tmpdir(), 'PetalCameraSoakAnimator.app');
  const binary = path.join(bundle, 'Contents', 'MacOS', 'PetalFreezeAnimator');
  fs.mkdirSync(path.dirname(binary), { recursive: true });
  fs.writeFileSync(path.join(bundle, 'Contents', 'Info.plist'), `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>PetalFreezeAnimator</string>
<key>CFBundleIdentifier</key><string>com.petal.testing.camera-soak-animator</string>
<key>CFBundleName</key><string>PetalFreezeAnimator</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>
`);
  const built = spawnSync('xcrun', ['swiftc', source, '-framework', 'AppKit', '-o', binary], {
    encoding: 'utf8',
    env: { ...process.env, SWIFT_MODULE_CACHE_PATH: path.join(os.tmpdir(), 'petal-camsoak-swift-cache'), CLANG_MODULE_CACHE_PATH: path.join(os.tmpdir(), 'petal-camsoak-clang-cache') },
  });
  if (built.status !== 0) throw new Error(`could not compile the animator: ${(built.stderr || built.stdout || '').trim()}`);
  const child = spawn(binary, [], {
    detached: true,
    stdio: ['ignore', 'pipe', 'pipe'],
    env: {
      ...process.env,
      PETAL_FREEZE_ANIMATOR_LOG: path.join(outDir, 'animator-ticks.log'),
      PETAL_FREEZE_ANIMATOR_STATIC_MS: String(shareStaticMs || 0),
    },
  });
  children.push({ name: 'animator', child });
  let stdout = '';
  child.stdout.setEncoding('utf8').on('data', (chunk) => { stdout += chunk; });
  await waitUntil('animator ready', () => {
    if (child.exitCode !== null) throw new Error(`animator exited ${child.exitCode}`);
    return stdout.includes('PETAL_FREEZE_ANIMATOR_READY');
  }, 20_000, 100);
  return child;
}

function chromeCommand() {
  return process.arch === 'arm64' ? ['arch', ['-arm64', chromeBin]] : [chromeBin, []];
}

async function startViewerBrowser(index) {
  const port = cdpBasePort + index;
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), `petal-camsoak-viewer-${index}-`));
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

// Runs inside each viewer page. The camera track is played in the soak's OWN
// <video> (a MediaStream around the subscribed MediaStreamTrack), so the
// measurement does not depend on how the deployed web build lays out tiles.
// rVFC gives every presented frame; inbound-rtp is sampled every 2 s.
function measurementScript(owner) {
  return `(() => {
  if (window.__cameraSoak) return 'already-installed';
  const OWNER = ${JSON.stringify(owner)};
  const S = { frames: [], stats: [], lag: [], events: [], track: null, armed: 0,
    shareFrames: [], shareStats: [], shareTrack: null, shareArmed: 0 };
  const origin = performance.timeOrigin;
  const video = document.createElement('video');
  video.muted = true; video.autoplay = true; video.playsInline = true;
  video.style.cssText = 'position:fixed;right:0;bottom:0;width:320px;height:180px;z-index:99999';
  document.body.appendChild(video);
  function arm() {
    const id = ++S.armed;
    const onFrame = (now, meta) => {
      if (id !== S.armed) return;
      const t = origin + (meta?.expectedDisplayTime ?? now);
      S.frames.push([Math.round(t * 10) / 10, meta?.rtpTimestamp ?? null, video.videoWidth, video.videoHeight,
        meta?.presentedFrames ?? null]);
      video.requestVideoFrameCallback(onFrame);
    };
    video.requestVideoFrameCallback(onFrame);
  }
  // A concurrent window share (--with-share) gets its own element and
  // timeline, so a share stall caused by the camera is visible too.
  const shareVideo = document.createElement('video');
  shareVideo.muted = true; shareVideo.autoplay = true; shareVideo.playsInline = true;
  shareVideo.style.cssText = 'position:fixed;left:0;bottom:0;width:320px;height:200px;z-index:99999';
  document.body.appendChild(shareVideo);
  function armShare() {
    const id = ++S.shareArmed;
    const onFrame = (now, meta) => {
      if (id !== S.shareArmed) return;
      S.shareFrames.push([Math.round((origin + (meta?.expectedDisplayTime ?? now)) * 10) / 10, meta?.rtpTimestamp ?? null,
        shareVideo.videoWidth, shareVideo.videoHeight]);
      shareVideo.requestVideoFrameCallback(onFrame);
    };
    shareVideo.requestVideoFrameCallback(onFrame);
  }
  function sharePublication() {
    const participant = window.__petalHarness?.room?.remoteParticipants?.get(OWNER);
    if (!participant) return null;
    for (const p of participant.trackPublications.values()) {
      if (String(p.trackName || '').startsWith('petal-window-')) return p;
    }
    return null;
  }
  function cameraPublication() {
    const room = window.__petalHarness?.room;
    const participant = room?.remoteParticipants?.get(OWNER);
    if (!participant) return null;
    for (const p of participant.trackPublications.values()) {
      if (String(p.trackName || '').startsWith('petal-camera-')) return p;
    }
    return null;
  }
  setInterval(() => {
    const pub = cameraPublication();
    if (pub && !pub.isSubscribed && pub.setSubscribed) pub.setSubscribed(true);
    const mst = pub?.track?.mediaStreamTrack ?? null;
    if (mst !== S.track) {
      S.events.push([Date.now(), mst ? 'camera-track-bound ' + pub.trackName : 'camera-track-gone']);
      S.track = mst;
      if (mst) {
        video.srcObject = new MediaStream([mst]);
        video.play().catch((e) => S.events.push([Date.now(), 'play-error ' + e]));
        arm();
      }
    }
    const spub = sharePublication();
    if (spub && !spub.isSubscribed && spub.setSubscribed) spub.setSubscribed(true);
    const smst = spub?.track?.mediaStreamTrack ?? null;
    if (smst !== S.shareTrack) {
      S.events.push([Date.now(), smst ? 'share-track-bound ' + spub.trackName : 'share-track-gone']);
      S.shareTrack = smst;
      if (smst) {
        shareVideo.srcObject = new MediaStream([smst]);
        shareVideo.play().catch((e) => S.events.push([Date.now(), 'share-play-error ' + e]));
        armShare();
      }
    }
  }, 250);
  const FIELDS = ['framesReceived', 'framesDecoded', 'framesDropped', 'framesPerSecond', 'frameWidth', 'frameHeight',
    'freezeCount', 'totalFreezesDuration', 'pauseCount', 'totalPausesDuration', 'jitterBufferDelay',
    'jitterBufferEmittedCount', 'packetsReceived', 'packetsLost', 'nackCount', 'pliCount',
    'firCount', 'keyFramesDecoded', 'totalDecodeTime', 'decoderImplementation', 'bytesReceived'];
  async function sample() {
    const pub = cameraPublication();
    const row = { t: Date.now(), subscribed: !!pub?.track, trackName: pub?.trackName ?? null };
    if (pub?.track?.getRTCStatsReport) {
      const report = await pub.track.getRTCStatsReport();
      report?.forEach((s) => {
        if (s.type === 'inbound-rtp' && s.kind === 'video') for (const f of FIELDS) if (s[f] !== undefined) row[f] = s[f];
        if (s.type === 'candidate-pair' && s.nominated && s.currentRoundTripTime !== undefined) row.rttMs = s.currentRoundTripTime * 1000;
        if (s.type === 'candidate-pair' && s.nominated && s.availableIncomingBitrate !== undefined) row.availInKbps = s.availableIncomingBitrate / 1000;
      });
    }
    S.stats.push(row);
    const spub = sharePublication();
    if (spub?.track?.getRTCStatsReport) {
      const srow = { t: Date.now(), trackName: spub.trackName };
      const report = await spub.track.getRTCStatsReport();
      report?.forEach((s) => {
        if (s.type === 'inbound-rtp' && s.kind === 'video') for (const f of FIELDS) if (s[f] !== undefined) srow[f] = s[f];
      });
      S.shareStats.push(srow);
    }
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
    const out = { frames: S.frames, stats: S.stats, lag: S.lag, events: S.events,
      shareFrames: S.shareFrames, shareStats: S.shareStats };
    S.frames = []; S.stats = []; S.lag = []; S.events = []; S.shareFrames = []; S.shareStats = [];
    return out;
  };
  window.__cameraSoak = S;
  return 'installed';
})()`;
}

async function drainViewer(viewer) {
  if (!viewer.cdp || viewer.dead) return;
  try {
    const batch = await evaluate(viewer.cdp, 'window.__cameraSoak ? window.__cameraSoak.drain() : null');
    if (!batch) throw new Error('measurement hook missing (page reloaded?)');
    viewer.record.frames.push(...batch.frames);
    viewer.record.stats.push(...batch.stats);
    viewer.record.lag.push(...batch.lag);
    viewer.record.events.push(...batch.events);
    viewer.record.shareFrames.push(...(batch.shareFrames ?? []));
    viewer.record.shareStats.push(...(batch.shareStats ?? []));
  } catch (error) {
    viewer.dead = true;
    raw.infra.push(`${viewer.record.label}: lost the measurement page mid-run: ${error.message}`);
  }
}

function senderRows(metrics) {
  const tracks = metrics?.network?.tracks ?? [];
  const pick = ['name', 'width', 'height', 'fps', 'codec_impl', 'quality_limitation', 'software_encoder', 'target_kbps',
    'actual_kbps', 'packets_lost', 'frames_encoded', 'key_frames_encoded', 'nack_count', 'fir_count', 'pli_count', 'rtt_ms'];
  const rows = [];
  for (const track of tracks) {
    if (track.direction !== 'send' || track.kind !== 'video') continue;
    const row = { t: Date.now() };
    for (const key of pick) {
      const camel = key.replace(/_([a-z])/g, (_, c) => c.toUpperCase());
      if (track[key] !== undefined) row[key] = track[key];
      else if (track[camel] !== undefined) row[key] = track[camel];
    }
    rows.push(row);
  }
  return rows;
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
  if (alive) console.log(`::warning::camera-freeze soak: ${name} (pid ${child.pid}) is still alive after SIGKILL`);
  if (profile) fs.rmSync(profile, { recursive: true, force: true });
  return !alive;
}

async function main() {
  if (!socketPath) throw new Error('no --socket / PETAL_AUTOTEST_SOCK');
  if (!Number.isFinite(durationMs) || durationMs < 30_000) throw new Error(`--duration-ms must be >= 30000 (got ${durationMs})`);
  if (!fs.existsSync(chromeBin)) throw new Error(`Chrome not found at ${chromeBin}`);

  const client = connectSocket(socketPath);
  const suffix = crypto.randomBytes(4).toString('hex');
  const identity = `p-camsoak-${suffix}`;
  let joined;
  try {
    joined = await command(client, { cmd: 'join', room: `camsoak-${suffix}`, identity, name: 'Camera Soak Sender' });
  } catch (error) {
    raw.infra.push(`native join to a fresh room failed (token mint / SFU connect): ${error.message}`);
    return;
  }
  const room = await command(client, { cmd: 'current_room' });
  if (!room?.accessCode) {
    raw.infra.push(`joined room exposes no access code (${JSON.stringify(room)}), so no viewer can join it`);
    return;
  }
  raw.sharer = { identity, livekitRoom: room.livekitRoom, joinedRoomId: joined?.id ?? null };
  note(`native sender joined livekitRoom=${room.livekitRoom} as ${identity}`);

  // Viewers join first so the camera's whole startup ramp is observed.
  const viewers = [];
  for (let i = 0; i < viewerCount; i += 1) {
    const label = `camera-viewer-${i + 1}`;
    const record = { label, identity: null, joinedAtMs: null, firstFrameAtMs: null, frames: [], stats: [], lag: [], events: [], shareFrames: [], shareStats: [] };
    raw.viewers.push(record);
    const viewer = { record, cdp: null, dead: false };
    viewers.push(viewer);
    try {
      const browser = await startViewerBrowser(i);
      viewer.cdp = await connectCdp(browser.wsUrl);
      const connected = await joinViewer(viewer.cdp, room.accessCode, label);
      if (connected.name && room.livekitRoom && connected.name !== room.livekitRoom) {
        throw new Error(`joined LiveKit room ${connected.name}, sender is in ${room.livekitRoom}`);
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

  try {
    const result = await command(client, { cmd: 'camera_on' });
    raw.cameraPublishedAtMs = Date.now();
    note(`camera_on -> ${JSON.stringify(result)}`);
    if (result?.published === false) {
      raw.infra.push('camera_on reported published=false (is PETAL_CAMERA_SYNTH_SOURCE=1 set?)');
      return;
    }
  } catch (error) {
    raw.infra.push(`camera_on failed: ${error.message}`);
    return;
  }

  let animator = null;
  let shared = null;
  if (withShare) {
    try {
      animator = await startAnimator();
      shared = await waitUntil('share the animator window', async () => {
        const response = await client.send({ cmd: 'share_matching', pid: animator.pid });
        return response.ok ? response.result : null;
      }, 20_000, 500);
      raw.sharer.windowId = shared.windowId;
      note(`also sharing animator window ${shared.windowId}`);
    } catch (error) {
      raw.infra.push(`--with-share: could not share the animator window: ${error.message}`);
      return;
    }
  }

  try {
    await waitUntil('every viewer presents a first camera frame', async () => {
      for (const viewer of viewers) {
        await drainViewer(viewer);
        if (!viewer.record.firstFrameAtMs && viewer.record.frames.length) viewer.record.firstFrameAtMs = viewer.record.frames[0][0];
      }
      return viewers.every((v) => v.record.firstFrameAtMs);
    }, 90_000, 1000);
  } catch (error) {
    const without = viewers.filter((v) => !v.record.firstFrameAtMs).map((v) => v.record.label);
    raw.infra.push(`no presented camera frame on ${without.join(', ')} within 90s of camera_on: ${error.message}`);
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
        raw.sender.push(...senderRows(await command(client, { cmd: 'dump_metrics', journal_tail: 0 })));
      } catch (error) {
        raw.fatal.push(`sender stopped answering the autotest socket at ${new Date(now).toISOString()}: ${error.message}`);
        break;
      }
    }
    if (now >= nextDrain) {
      nextDrain = now + 5000;
      for (const viewer of viewers) await drainViewer(viewer);
      if (viewers.some((v) => v.dead)) break;
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
    if (shared) await command(client, { cmd: 'stop_share', window_id: shared.windowId });
    await command(client, { cmd: 'camera_off' });
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
  for (const entry of children.reverse()) await stopChild(entry);
  const file = path.join(outDir, 'camera-freeze-raw.json');
  fs.writeFileSync(file, `${JSON.stringify(raw)}\n`);
  console.log(`camera-freeze raw evidence: ${file} (infra=${raw.infra.length} fatal=${raw.fatal.length})`);
  for (const problem of raw.infra) console.log(`::warning::camera-freeze soak infra: ${problem}`);
  for (const problem of raw.fatal) console.log(`::error::camera-freeze soak: ${problem}`);
  if (raw.infra.length || raw.fatal.length) exitCode = 2;
}
process.exitCode = exitCode;
