#!/usr/bin/env node
// #377 live check: a usage-stats opt-out sends NOTHING to PostHog.
//
// Petal must be launched with PETAL_POSTHOG_KEY=phc_<anything> and
// PETAL_POSTHOG_HOST=http://127.0.0.1:<port>; this script serves that port as
// a mock ingest endpoint and records every POST. Three phases, all through
// the autotest socket's `set_usage_stats` (the Settings toggle's command):
//   1. ON  (positive control): the share cycle's share_started AND
//      share_stopped must both reach the mock, or the
//      opt-out proof below would be vacuous.
//   2. OFF: share churn for --off-seconds; any POST at all is a failure.
//   3. ON again: both cycle events must arrive again (gate not stuck closed).
// Consent is left ON at exit. Exit 0 = pass, 1 = fail, 2 = could not run.
import { writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import process from 'node:process';

import { connectSocket } from './autotest-socket.mjs';

function arg(name, fallback) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : fallback;
}

const socketPath = process.env.PETAL_AUTOTEST_SOCK;
const port = Number(arg('--port', process.env.PETAL_POSTHOG_CONSENT_PORT ?? ''));
const offSeconds = Number(arg('--off-seconds', '30'));
const jsonOut = arg('--json', '');
const SETTLE_MS = 3000;
const CONTROL_WAIT_MS = 15000;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const log = (line) => console.log(`posthog-consent: ${line}`);

function finish(code, verdict, details) {
  const summary = { verdict, ...details };
  if (jsonOut) writeFileSync(jsonOut, `${JSON.stringify(summary, null, 2)}\n`);
  log(`verdict=${verdict} ${JSON.stringify(details)}`);
  process.exit(code);
}

if (!socketPath || !Number.isInteger(port) || port <= 0) {
  finish(2, 'CANNOT-RUN', { reason: 'need PETAL_AUTOTEST_SOCK and --port / PETAL_POSTHOG_CONSENT_PORT' });
}

const posts = [];
const server = createServer((req, res) => {
  let body = '';
  req.setEncoding('utf8');
  req.on('data', (chunk) => (body += chunk));
  req.on('end', () => {
    if (req.method === 'POST') {
      let event = 'unparsed';
      try {
        event = JSON.parse(body).event ?? 'missing';
      } catch {
        // keep 'unparsed'; any POST counts regardless
      }
      posts.push({ at: Date.now(), path: req.url, event });
    }
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end('{"status":1}');
  });
});
await new Promise((resolve, reject) => {
  server.once('error', reject);
  server.listen(port, '127.0.0.1', resolve);
}).catch((error) => finish(2, 'CANNOT-RUN', { reason: `mock endpoint did not bind :${port}: ${error.message}` }));

const client = connectSocket(socketPath);
async function command(payload) {
  const response = await client.send(payload);
  if (!response.ok) throw new Error(`${payload.cmd} failed: ${response.error}`);
  return response.result;
}

async function shareCycle() {
  const shared = await command({ cmd: 'share_display' });
  await sleep(1500);
  await command({ cmd: 'stop_share', window_id: shared.windowId });
  await sleep(500);
}

// The share cycle's own events: a stray event (e.g. a late
// remote_control_input) must not stand in for proof the pipe is live.
const CYCLE_EVENTS = ['share_started', 'share_stopped'];

async function waitForCycleEventsSince(since, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  const seen = () => posts.filter((post) => post.at >= since);
  const complete = () => CYCLE_EVENTS.every((name) => seen().some((post) => post.event === name));
  while (Date.now() < deadline && !complete()) await sleep(250);
  return { posts: seen(), complete: complete() };
}

const details = { port, offSeconds };
try {
  // Phase 1: positive control.
  await command({ cmd: 'set_usage_stats', enabled: true });
  const controlStart = Date.now();
  await shareCycle();
  const control = await waitForCycleEventsSince(controlStart, CONTROL_WAIT_MS);
  details.onPosts = control.posts.length;
  details.onEvents = [...new Set(control.posts.map((post) => post.event))];
  log(`phase ON: ${control.posts.length} POST(s) ${JSON.stringify(details.onEvents)}`);
  if (!control.complete) {
    finish(2, 'CANNOT-RUN', {
      ...details,
      reason: `share_started + share_stopped did not both reach the mock with consent ON -- the pipe is not proven live, so an opt-out proof would be vacuous`,
    });
  }

  // Phase 2: opted out.
  const off = await command({ cmd: 'set_usage_stats', enabled: false });
  if (off.usageStatsEnabled !== false) throw new Error(`set_usage_stats false reported ${JSON.stringify(off)}`);
  await sleep(SETTLE_MS);
  const offStart = Date.now();
  let cycles = 0;
  while (Date.now() - offStart < offSeconds * 1000) {
    await shareCycle();
    cycles += 1;
  }
  await sleep(SETTLE_MS);
  const leaked = posts.filter((post) => post.at >= offStart);
  details.offCycles = cycles;
  details.offWindowMs = Date.now() - offStart;
  details.offPosts = leaked.length;
  details.offEvents = leaked.map((post) => post.event);
  log(`phase OFF: ${cycles} share cycle(s) over ${details.offWindowMs}ms, ${leaked.length} POST(s)`);

  // Phase 3: opted back in.
  await command({ cmd: 'set_usage_stats', enabled: true });
  const resumeStart = Date.now();
  await shareCycle();
  const resumed = await waitForCycleEventsSince(resumeStart, CONTROL_WAIT_MS);
  details.resumedPosts = resumed.posts.length;
  details.resumedEvents = [...new Set(resumed.posts.map((post) => post.event))];
  log(`phase ON again: ${resumed.posts.length} POST(s) ${JSON.stringify(details.resumedEvents)}`);

  if (leaked.length > 0) finish(1, 'FAIL', { ...details, reason: 'PostHog POSTs arrived while usage stats were OFF' });
  if (!resumed.complete) finish(1, 'FAIL', { ...details, reason: 'share_started + share_stopped did not resume after opting back in' });
  finish(0, 'PASS', details);
} catch (error) {
  try {
    await client.send({ cmd: 'set_usage_stats', enabled: true });
  } catch {
    // best effort: leave consent ON for anything that runs after us
  }
  finish(2, 'CANNOT-RUN', { ...details, reason: error instanceof Error ? error.message : String(error) });
} finally {
  client.close();
  server.close();
}
