#!/usr/bin/env node
// End-to-end test for scripts/measure-camera-intent.mjs against a FAKE Petal:
// a Unix socket that speaks the autotest wire shape and writes the same log
// lines the real camera_session.rs / Settings.svelte emit. Proves the driver
// walks #76's runbook in order, waits on the right markers, and hands the
// analyzer a log it reads as a contended first-attempt win.
//
// NOT covered here, by construction: the real contention. That needs a Mac
// with a camera and a live Settings preview -- which is the whole point of
// the driver existing.

import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { appendFileSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import net from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const DRIVER = fileURLToPath(new URL('./measure-camera-intent.mjs', import.meta.url));

function stamp() {
  return new Date().toISOString().replace('T', ' ').replace('Z', '');
}

class FakePetal {
  constructor(dir, { inRoom = true } = {}) {
    this.log = join(dir, 'petal.log.2026-09-12');
    this.socketPath = join(dir, 'p.sock');
    this.commands = [];
    this.publishing = false;
    this.intended = false;
    this.room = inRoom ? 'room-fake' : null;
    writeFileSync(
      this.log,
      `${stamp()} [INFO] [desktop_lib] petal: startup build identity -- version=0.9.26 commit=0123abcd build_date=2026-09-12 bundle_id=com.petal.app\n`
    );
  }

  line(message) {
    appendFileSync(this.log, `${stamp()} [INFO] [desktop_lib::camera_session] ${message}\n`);
  }

  publish() {
    this.line('session: camera-intent intended=true');
    this.line("session: start_camera_publish begin (identity '<redacted:i1>')");
    this.line('settings: camera preview released reason=meeting-camera');
    this.line('session: start_camera_publish capture running, waiting for first frame');
    this.line('session: start_camera_publish succeeded (1280x720)');
    this.publishing = true;
    this.intended = true;
  }

  stop() {
    this.line('session: stop_camera_publish begin');
    this.line('session: stop_camera_publish done (camera released)');
  }

  handle(command) {
    this.commands.push(command.cmd);
    switch (command.cmd) {
      case 'dump_state':
        // camelCase, as serde renames `DumpState` on the wire.
        return { currentRoom: this.room };
      case 'join':
        this.room = 'room-joined';
        return { room: this.room };
      case 'leave':
        this.room = null;
        return { left: true };
      case 'camera_state':
        return { publishing: this.publishing, intended: this.intended };
      case 'open_settings':
        this.line('settings: camera preview acquired');
        return { opened: true };
      case 'close_settings':
        this.line('settings: camera preview released reason=teardown');
        return { closed: true };
      case 'camera_on':
        this.publish();
        return { published: true };
      case 'camera_off':
        this.stop();
        this.publishing = false;
        this.intended = false;
        this.line('session: camera-intent intended=false');
        this.line('settings: camera preview acquired');
        return { stopped: true };
      case 'list_camera_devices':
        return { devices: [{ id: 'cam-a', name: 'A' }, { id: 'cam-b', name: 'B' }] };
      case 'set_camera_device':
        assert.ok(command.device_id, 'set_camera_device carries a device_id');
        this.stop();
        this.publish();
        return { applied: true, inRoom: true, usedDefaultFallback: false, error: null };
      default:
        throw new Error(`unexpected command ${command.cmd}`);
    }
  }

  listen() {
    this.server = net.createServer((socket) => {
      let buffer = '';
      socket.setEncoding('utf8');
      socket.on('data', (chunk) => {
        buffer += chunk;
        let idx;
        while ((idx = buffer.indexOf('\n')) >= 0) {
          const line = buffer.slice(0, idx);
          buffer = buffer.slice(idx + 1);
          if (!line.trim()) continue;
          let response;
          try {
            response = { ok: true, result: this.handle(JSON.parse(line)) };
          } catch (error) {
            response = { ok: false, error: error.message };
          }
          socket.write(`${JSON.stringify(response)}\n`);
        }
      });
    });
    return new Promise((resolve) => this.server.listen(this.socketPath, resolve));
  }

  close() {
    return new Promise((resolve) => this.server.close(resolve));
  }
}

// Asynchronous on purpose: the fake app lives in THIS process, so a blocking
// spawnSync would stall its event loop and it could never answer the driver.
function runDriver(fake, extra = []) {
  return new Promise((resolve, reject) => {
    const child = spawn(
      process.execPath,
      [DRIVER, '--socket', fake.socketPath, '--log', fake.log, '--settle-ms', '0', ...extra],
      { stdio: ['ignore', 'pipe', 'pipe'] }
    );
    let stdout = '';
    let stderr = '';
    child.stdout.setEncoding('utf8').on('data', (chunk) => (stdout += chunk));
    child.stderr.setEncoding('utf8').on('data', (chunk) => (stderr += chunk));
    child.on('error', reject);
    child.on('close', (status) => resolve({ status, stdout, stderr }));
  });
}

test('the driver walks the runbook in order and the analyzer reads a contended first-attempt win', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'p76-'));
  const fake = new FakePetal(dir);
  await fake.listen();
  try {
    const result = await runDriver(fake);
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
    assert.deepEqual(
      fake.commands.filter((cmd) => cmd !== 'camera_state'),
      [
        'dump_state',
        'open_settings',
        'camera_on',
        'camera_off',
        'list_camera_devices',
        'camera_on',
        'set_camera_device',
        'set_camera_device',
        'camera_off',
        'close_settings',
      ]
    );
    assert.match(result.stdout, /preview live before the first toggle: yes/);
    assert.match(result.stdout, /device switch: done/);
    assert.match(result.stdout, /VERDICT: first-attempt-wins/);
    assert.match(result.stdout, /CONTENDED: 2 of 4 measured episode\(s\)/);
    assert.match(result.stdout, /device switch x2/);
    // Paste-safe end to end: nothing the fake wrote reaches the output.
    assert.doesNotMatch(result.stdout, /redacted:i1|room-fake|cam-a|meeting-camera/);
  } finally {
    await fake.close();
    rmSync(dir, { recursive: true, force: true });
  }
});

test('the driver joins a room on request, and refuses to run without one', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'p76-'));
  const fake = new FakePetal(dir, { inRoom: false });
  await fake.listen();
  try {
    const refused = await runDriver(fake, ['--no-analyze']);
    assert.equal(refused.status, 1);
    assert.match(refused.stderr, /not in a room/);
    assert.deepEqual(fake.commands, ['dump_state']);

    fake.commands.length = 0;
    const joined = await runDriver(fake, ['--no-analyze', '--room', 'qa-key-1', '--keep-settings']);
    assert.equal(joined.status, 0, `${joined.stdout}\n${joined.stderr}`);
    assert.equal(fake.commands[0], 'dump_state');
    assert.equal(fake.commands[1], 'join');
    assert.equal(fake.commands.at(-1), 'leave', 'a room the driver joined is left again');
    assert.ok(!fake.commands.includes('close_settings'), '--keep-settings leaves Settings open');
  } finally {
    await fake.close();
    rmSync(dir, { recursive: true, force: true });
  }
});

test('a preview that never goes live is reported, not hidden', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'p76-'));
  const fake = new FakePetal(dir);
  // Settings opens, but no `preview acquired` line ever lands (no camera
  // permission, say). The driver must say so and still finish the runbook.
  const originalHandle = fake.handle.bind(fake);
  fake.handle = (command) => {
    if (command.cmd === 'open_settings') {
      fake.commands.push(command.cmd);
      return { opened: true };
    }
    return originalHandle(command);
  };
  await fake.listen();
  try {
    const result = await runDriver(fake, ['--no-analyze', '--preview-timeout-ms', '300']);
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
    assert.match(result.stdout, /no `settings: camera preview acquired` line within 300 ms/);
    assert.match(result.stdout, /preview live before the first toggle: NO/);
  } finally {
    await fake.close();
    rmSync(dir, { recursive: true, force: true });
  }
});
