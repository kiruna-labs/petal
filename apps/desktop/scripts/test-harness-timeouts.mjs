#!/usr/bin/env node
// #102: the live loopback tier must never again wedge without a bound.
//
// The defect these tests pin: `connectSocket().send()` settled only when a
// newline-terminated response arrived and rejected only on a socket `error`.
// It handled neither a timeout nor a clean `close`/`end`. On 2026-09-08 the app
// stopped answering during the #298 resume/reconnect simulation while the
// socket stayed open, the suite blocked forever, and one run held the only
// self-hosted macOS runner for 81 minutes with a release gate queued behind it.
//
// Every test below drives a REAL Unix socket server that deliberately
// misbehaves, rather than asserting on the parsing helper in isolation -- the
// bug lived in the wiring between the socket's events and the promise, and a
// test of the parser alone would have passed throughout.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';

import { connectSocket } from './autotest-socket.mjs';
import {
  COMMAND_TIMEOUT_ENV,
  DEFAULT_COMMAND_TIMEOUT_MS,
  DEFAULT_RUN_TIMEOUT_MS,
  RUN_TIMEOUT_ENV,
  commandTimeoutMs,
  runTimeoutMs,
} from './harness-timeouts.mjs';
import { captureWedgeEvidence } from './harness-wedge-evidence.mjs';

const tempDirs = [];
function tempDir() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'petal-102-'));
  tempDirs.push(dir);
  return dir;
}
after(() => {
  for (const dir of tempDirs) fs.rmSync(dir, { recursive: true, force: true });
});

/// Starts a Unix socket server whose connection handler is `onConnection`, and
/// returns its path. `onConnection` that does nothing models the wedge.
function startServer(onConnection) {
  const dir = tempDir();
  const sockPath = path.join(dir, 'petal-rc.sock');
  const server = net.createServer(onConnection);
  return new Promise((resolve) => {
    server.listen(sockPath, () => resolve({ sockPath, server }));
  });
}

test('a command that never gets a response rejects instead of hanging forever', async () => {
  // The exact 2026-09-08 shape: the connection is accepted, the socket stays
  // open, and no response is ever written.
  const { sockPath, server } = await startServer(() => {});
  const wedges = [];
  const client = connectSocket(sockPath, {
    timeoutMs: 150,
    onWedge: (reason) => wedges.push(reason),
  });
  const started = Date.now();
  await assert.rejects(
    () => client.send({ cmd: 'reconnect', mode: 'resume' }),
    (error) => {
      assert.match(error.message, /reconnect/);
      assert.match(error.message, /150ms/);
      assert.match(error.message, /#102/);
      return true;
    }
  );
  assert.ok(
    Date.now() - started < 5_000,
    'the command must fail on its own timeout, not sit until an external kill'
  );
  assert.equal(wedges.length, 1, 'a timeout must capture wedge evidence exactly once');
  assert.match(wedges[0], /stopped answering the autotest socket/);
  server.close();
});

test('a healthy command still resolves and is never charged a timeout', async () => {
  const { sockPath, server } = await startServer((socket) => {
    socket.on('data', () => socket.write(`${JSON.stringify({ ok: true, result: { pong: 1 } })}\n`));
  });
  const client = connectSocket(sockPath, { timeoutMs: 5_000, onWedge: () => {} });
  const response = await client.send({ cmd: 'dump_state' });
  assert.deepEqual(response, { ok: true, result: { pong: 1 } });
  // Direction 2: a gate whose negative result carries no information is worth
  // nothing. The same client must still answer a second command normally.
  assert.deepEqual(await client.send({ cmd: 'dump_state' }), { ok: true, result: { pong: 1 } });
  client.close();
  server.close();
});

test('a socket the app closes mid-command rejects rather than never settling', async () => {
  // No `error` event is emitted for a clean FIN, so before #102 this promise
  // simply never settled.
  const { sockPath, server } = await startServer((socket) => {
    socket.on('data', () => socket.end());
  });
  const client = connectSocket(sockPath, { timeoutMs: 30_000, onWedge: () => {} });
  await assert.rejects(
    () => client.send({ cmd: 'remote-control-status', window_id: 1 }),
    /closed by the app|ended by the app|socket error/
  );
  server.close();
});

test('after a wedge every later command fails immediately instead of hanging too', async () => {
  const { sockPath, server } = await startServer(() => {});
  const client = connectSocket(sockPath, { timeoutMs: 100, onWedge: () => {} });
  await assert.rejects(() => client.send({ cmd: 'reconnect', mode: 'resume' }));
  const started = Date.now();
  await assert.rejects(
    () => client.send({ cmd: 'remote-control-status', window_id: 1 }),
    (error) => {
      assert.match(error.message, /no longer usable/);
      assert.match(error.message, /remote-control-status/);
      return true;
    }
  );
  assert.ok(
    Date.now() - started < 100,
    'a dead socket must fail the next command at once, not re-serve the full timeout'
  );
  server.close();
});

test('evidence capture never throws, and reports when there is nothing to capture', () => {
  const dir = tempDir();
  const written = captureWedgeEvidence('unit test: no app is running', { dir, pids: [] });
  assert.ok(written.length >= 1, 'the reason file must always be written');
  const reason = fs.readFileSync(written[0], 'utf8');
  assert.match(reason, /unit test: no app is running/);
  assert.match(reason, /desktop pids: \(none running\)/);
});

test('evidence capture includes the app log tail so a reader sees the last thing it said', () => {
  const dir = tempDir();
  const devLog = path.join(dir, 'petal-dev.log');
  fs.writeFileSync(devLog, 'first line\nlast thing the app said before the wedge\n');
  const written = captureWedgeEvidence('unit test: log tail', { dir, pids: [], devLog });
  const tail = written.find((f) => f.endsWith('-petal-dev-tail.log'));
  assert.ok(tail, `expected a log tail among ${JSON.stringify(written)}`);
  assert.match(fs.readFileSync(tail, 'utf8'), /last thing the app said before the wedge/);
});

test('timeout defaults are the documented ones and env overrides are validated', () => {
  assert.equal(commandTimeoutMs({}), DEFAULT_COMMAND_TIMEOUT_MS);
  assert.equal(runTimeoutMs({}), DEFAULT_RUN_TIMEOUT_MS);
  assert.equal(commandTimeoutMs({ [COMMAND_TIMEOUT_ENV]: '1234' }), 1234);
  assert.equal(runTimeoutMs({ [RUN_TIMEOUT_ENV]: '4321' }), 4321);
  // A typo'd override must fail loudly rather than silently disabling the
  // bound this whole issue is about.
  assert.throws(() => commandTimeoutMs({ [COMMAND_TIMEOUT_ENV]: 'soon' }), /positive number/);
  assert.throws(() => runTimeoutMs({ [RUN_TIMEOUT_ENV]: '0' }), /positive number/);
  assert.throws(() => runTimeoutMs({ [RUN_TIMEOUT_ENV]: '-1' }), /positive number/);
});

test('the loopback job and its harness step both carry a timeout', () => {
  // The bound has to exist in the WORKFLOW too: the harness timeout cannot
  // free the runner if the wedge is outside the harness process.
  const workflow = fs.readFileSync(
    path.join(path.dirname(new URL(import.meta.url).pathname), '../../../.github/workflows/nightly-loopback.yml'),
    'utf8'
  );
  const job = workflow.slice(workflow.indexOf('  live-loopback:'));
  const jobTimeout = job.match(/^    timeout-minutes: (\d+)$/m);
  assert.ok(jobTimeout, 'the live-loopback job must declare timeout-minutes (#102)');
  assert.ok(
    Number(jobTimeout[1]) <= 90,
    `a job cap above 90 minutes does not bound the incident this fixes (got ${jobTimeout[1]})`
  );
  const step = job.slice(job.indexOf('- name: Run live loopback harness'));
  const stepTimeout = step.match(/^        timeout-minutes: (\d+)$/m);
  assert.ok(stepTimeout, 'the "Run live loopback harness" step must declare timeout-minutes (#102)');
  assert.ok(
    Number(stepTimeout[1]) < Number(jobTimeout[1]),
    'the step cap must fire before the job cap so the failure names the step that wedged'
  );
});
