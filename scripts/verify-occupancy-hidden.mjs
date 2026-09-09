#!/usr/bin/env node
// #120: does "N people in the room" count PEOPLE?
//
// The room card's number is `POST /api/rooms/status` -> `occupancy`. It used
// to be LiveKit's room-level `numParticipants`, which COUNTS HIDDEN
// participants -- and every desktop user opens a hidden `-gallery` bridge
// connection, so a room of N people with k desktop users read N + k. Measured
// 2026-09-09 on livekit-server 1.13.2 and on the hosted LiveKit Cloud
// deployment; the code had asserted the opposite in three places.
//
// The backend unit tests pin the counting rule against mocks. This script
// pins it against a REAL SFU: real join handshakes, a real hidden-token mint
// through /api/gallery-token's trust anchor, and the real handler reading a
// real RoomServiceClient. A mock cannot tell you that `permission.hidden` is
// actually populated by the server on the wire.
//
// Tier 2 (needs-local-livekit). NOT part of scripts/ci-local.sh -- it starts
// a server and a browser.
//
// Prerequisites:
//   livekit-server on PATH        (brew install livekit)
//   cd backend && npm ci          (tsx + livekit-server-sdk)
//   cd apps/desktop && npm ci     (Playwright + its Chromium)
//   cd web-harness && npm ci      (livekit-client UMD)
//
// Run:  node scripts/verify-occupancy-hidden.mjs
// Exits 0 on pass, 1 on a failed assertion, 2 if a prerequisite is missing.

import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
import net from 'node:net';
import { pathToFileURL } from 'node:url';
import { resolve } from 'node:path';

const repoRoot = resolve(import.meta.dirname, '..');
const require_ = createRequire(import.meta.url);

function missing(what, error) {
  console.error(`${what} unavailable: ${error instanceof Error ? error.message : error}`);
  process.exit(2);
}

let chromium;
try {
  const playwrightModule =
    process.env.PETAL_PLAYWRIGHT_MODULE ?? resolve(repoRoot, 'apps/desktop/node_modules/playwright');
  ({ chromium } = require_(playwrightModule));
} catch (error) {
  missing('Playwright (cd apps/desktop && npm ci)', error);
}

let livekitClientUmd;
try {
  livekitClientUmd = readFileSync(
    resolve(repoRoot, 'web-harness/node_modules/livekit-client/dist/livekit-client.umd.js'),
    'utf8'
  );
} catch (error) {
  missing('livekit-client UMD build (cd web-harness && npm ci)', error);
}

// The handlers are TypeScript; run them through backend's own tsx loader so
// this script exercises the SAME code the deployed function does.
const backendRequire = createRequire(resolve(repoRoot, 'backend/package.json'));
try {
  const tsxApi = await import(pathToFileURL(backendRequire.resolve('tsx/esm/api')).href);
  tsxApi.register();
} catch (error) {
  missing('tsx (cd backend && npm ci)', error);
}

async function freePort() {
  return new Promise((resolvePort, reject) => {
    const probe = net.createServer();
    probe.on('error', reject);
    probe.listen(0, '127.0.0.1', () => {
      const { port } = probe.address();
      probe.close(() => resolvePort(port));
    });
  });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const failures = [];
function check(desc, actual, expected) {
  if (actual === expected) {
    console.log(`ok   ${desc} (occupancy=${actual})`);
  } else {
    failures.push(desc);
    console.error(`FAIL ${desc}: expected occupancy=${expected}, got ${actual}`);
  }
}

// The SFU's room-level `numParticipants` trails its participant list by
// seconds (measured: still 1 while the roster already held 3, catching up
// within ~7s), and a room the list still calls empty is deliberately not
// fanned out to -- so a freshly joined room can read 0 for a beat. Settle
// first, then assert, and assert AGAIN after the value stops moving so
// "briefly correct" cannot pass for correct.
async function settle(read, expected, timeoutMs = 25_000) {
  const deadline = Date.now() + timeoutMs;
  let value = await read();
  while (value !== expected && Date.now() < deadline) {
    await sleep(1_000);
    value = await read();
  }
  return value;
}

const httpPort = await freePort();
const rtcPort = await freePort();
const API_KEY = 'devkey';
const API_SECRET = 'secret-at-least-32-characters-long-ok';

const server = spawn(
  'livekit-server',
  [
    '--bind',
    '127.0.0.1',
    '--node-ip',
    '127.0.0.1',
    '--config-body',
    [
      `port: ${httpPort}`,
      'rtc:',
      `  tcp_port: ${rtcPort}`,
      '  use_external_ip: false',
      'keys:',
      `  ${API_KEY}: ${API_SECRET}`,
      'logging:',
      '  level: error',
    ].join('\n'),
  ],
  { stdio: ['ignore', 'pipe', 'pipe'] }
);
let serverLog = '';
server.stdout.on('data', (d) => (serverLog += d));
server.stderr.on('data', (d) => (serverLog += d));
server.on('exit', (code) => {
  if (code !== 0 && code !== null) {
    console.error(`livekit-server exited with ${code}:\n${serverLog}`);
  }
});

// The env the handlers read. Set BEFORE importing them.
process.env.LIVEKIT_URL = `ws://127.0.0.1:${httpPort}`;
process.env.LIVEKIT_API_KEY = API_KEY;
process.env.LIVEKIT_API_SECRET = API_SECRET;

// Serve the join page from a localhost origin: Chromium's local-network-access
// check blocks a non-local page origin from reaching 127.0.0.1.
const pageServer = createServer((req, res) => {
  if (req.url?.startsWith('/livekit-client.umd.js')) {
    res.writeHead(200, { 'content-type': 'application/javascript' });
    res.end(livekitClientUmd);
    return;
  }
  res.writeHead(200, { 'content-type': 'text/html' });
  res.end('<!doctype html><meta charset="utf-8"><title>occupancy probe</title><script src="/livekit-client.umd.js"></script>');
});
await new Promise((r) => pageServer.listen(0, '127.0.0.1', r));
const pageOrigin = `http://127.0.0.1:${pageServer.address().port}`;

let browser;
let exitCode = 0;
try {
  // Wait for the SFU's HTTP listener.
  const deadline = Date.now() + 30_000;
  for (;;) {
    try {
      await fetch(`http://127.0.0.1:${httpPort}/`);
      break;
    } catch {
      if (Date.now() > deadline) throw new Error(`livekit-server did not start:\n${serverLog}`);
      await sleep(250);
    }
  }

  const { handleCreateRoom, handleGalleryToken, handleRoomStatus, handleToken, ROOMS_LIST_CACHE_MS } =
    await import(pathToFileURL(resolve(repoRoot, 'backend/lib/handlers.ts')).href);

  // A virtual clock that always steps past the 3s status cache, so every
  // reading below is a fresh RPC rather than a cached one.
  let clock = Date.now();
  const tick = () => (clock += ROOMS_LIST_CACHE_MS + 1);

  const created = await handleCreateRoom({ name: 'Occupancy probe', open: true }, { nowMs: tick() });
  const credential = created.room.slug;

  browser = await chromium.launch({ headless: true });
  const joined = [];
  async function join(token, label) {
    const page = await browser.newPage();
    await page.goto(pageOrigin, { waitUntil: 'load' });
    const state = await page.evaluate(
      async ({ url, token }) => {
        const room = new window.LivekitClient.Room();
        window.__room = room;
        await room.connect(url, token, { autoSubscribe: true });
        return room.state;
      },
      { url: process.env.LIVEKIT_URL, token }
    );
    if (state !== 'connected') throw new Error(`${label} failed to connect (state=${state})`);
    joined.push({ page, label });
    // The SFU's room-level counters trail a join by a beat.
    await sleep(1_500);
    return page;
  }
  async function leave(label) {
    const entry = joined.find((j) => j.label === label);
    if (!entry) throw new Error(`no joined peer named ${label}`);
    await entry.page.evaluate(() => window.__room?.disconnect());
    joined.splice(joined.indexOf(entry), 1);
    await entry.page.close();
    await sleep(2_500);
  }
  async function occupancy() {
    const { rooms } = await handleRoomStatus({ rooms: [{ room: credential }] }, { nowMs: tick() });
    return rooms[0]?.occupancy;
  }
  async function stableOccupancy(expected) {
    const settled = await settle(occupancy, expected);
    if (settled !== expected) return settled;
    await sleep(2_000);
    return occupancy();
  }

  const alice = crypto.randomUUID();
  const bob = crypto.randomUUID();

  const aliceToken = await handleToken({ room: credential, identity: alice, displayName: 'Alice' }, { nowMs: tick() });
  await join(aliceToken.token, 'alice');
  check('one visible participant reads 1', await stableOccupancy(1), 1);

  // The real trust-anchored mint: it requires Alice to be connected already.
  const aliceBridge = await handleGalleryToken(
    { room: credential, baseIdentity: alice, displayName: 'Alice gallery' },
    { nowMs: tick() }
  );
  await join(aliceBridge.token, 'alice-gallery');
  check('a desktop user and their hidden -gallery bridge still read 1', await stableOccupancy(1), 1);

  const bobToken = await handleToken({ room: credential, identity: bob, displayName: 'Bob' }, { nowMs: tick() });
  await join(bobToken.token, 'bob');
  check('two people, one bridge reads 2', await stableOccupancy(2), 2);

  await leave('alice');
  check('a graceful leave drops the count, the orphaned bridge is still not a person', await stableOccupancy(1), 1);

  await leave('bob');
  check('a room holding only a hidden bridge reads 0, never 1', await stableOccupancy(0), 0);

  // Documented, NOT asserted: an ungraceful drop (page killed without
  // disconnect) stays counted for the SFU's reconnect grace, ~20-30s
  // measured. That window is accepted -- see docs/CONTRACTS.md.
} catch (error) {
  failures.push(`threw: ${error instanceof Error ? error.stack : error}`);
  console.error(error);
} finally {
  if (browser) await browser.close().catch(() => {});
  pageServer.close();
  server.kill('SIGTERM');
}

if (failures.length) {
  console.error(`\n${failures.length} check(s) failed`);
  exitCode = 1;
} else {
  console.log('\nall occupancy checks passed');
}
process.exit(exitCode);
