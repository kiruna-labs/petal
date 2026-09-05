#!/usr/bin/env node
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, '../../..');
const preflightPath = path.join(scriptDir, 'remote-control-harness-preflight.mjs');
const loopbackPath = path.join(scriptDir, 'remote-control-local-loopback.mjs');
const sentinelPath = path.join(scriptDir, 'remote-control-photon-sentinel.swift');

function run(script, args, env = {}) {
  return spawnSync(process.execPath, [script, ...args], {
    cwd: repoRoot,
    encoding: 'utf8',
    env: { ...process.env, ...env },
  });
}

function withFakeXcrun(mode, fn) {
  const tempDir = mkdtempSync(path.join(os.tmpdir(), 'petal-preflight-test-'));
  const binDir = path.join(tempDir, 'bin');
  const tracePath = path.join(tempDir, 'xcrun-args.txt');
  mkdirSync(binDir);
  const xcrunPath = path.join(binDir, 'xcrun');
  writeFileSync(
    xcrunPath,
    '#!/bin/sh\nprintf "%s\\n" "$*" > "$PETAL_TEST_XCRUN_TRACE"\nif [ "$PETAL_TEST_XCRUN_MODE" = "fail" ]; then exit 41; fi\n',
    { mode: 0o755 }
  );
  try {
    return fn({
      PATH: `${binDir}${path.delimiter}${process.env.PATH}`,
      PETAL_TEST_XCRUN_MODE: mode,
      PETAL_TEST_XCRUN_TRACE: tracePath,
      tracePath,
    });
  } finally {
    rmSync(tempDir, { recursive: true, force: true });
  }
}

function assertPortableChecks(output) {
  const normalized = output.replaceAll('\\', '/');
  for (const marker of [
    /ok all v1 and v2 variants/,
    /ok apps\/desktop\/src-tauri\/src\/remote_control\.rs carries/,
    /ok web-harness\/tests\/contracts\.test\.ts carries/,
    /ok web-harness\/tests\/remoteControl\.test\.ts carries/,
    /ok web-harness\/src\/harnessApi\.ts carries/,
    /ok apps\/desktop\/scripts\/remote-control-local-loopback\.mjs carries/,
    /ok apps\/desktop\/scripts\/remote-control-scenario\.mjs carries/,
    /ok web-harness\/src\/remoteControlPhoton\.ts carries/,
    /ok web-harness\/tests\/remoteControlPhoton\.test\.ts carries/,
    /ok apps\/desktop\/scripts\/remote-control-photon-sentinel\.swift carries/,
    /ok apps\/desktop\/scripts\/remote-control-photon-metrics\.mjs carries/,
  ]) {
    assert.match(normalized, marker);
  }
  assert.match(normalized, /DEFERRED Swift\/AppKit sentinel: exercised by the named macOS CI gate/);
}

test('portable check-only preflight runs every portable check without xcrun', () => {
  withFakeXcrun('success', (env) => {
    const result = run(preflightPath, ['--check-only', '--skip-swift-typecheck'], env);
    assert.equal(result.status, 0, result.stderr);
    assertPortableChecks(result.stdout);
    assert.equal(existsSync(env.tracePath), false, 'portable path must not invoke xcrun');
  });
});

test('default check-only preflight invokes the exact Swift sentinel command', { skip: process.platform === 'win32' }, () => {
  withFakeXcrun('success', (env) => {
    const result = run(preflightPath, ['--check-only'], env);
    assert.equal(result.status, 0, result.stderr);
    assert.equal(readFileSync(env.tracePath, 'utf8').trim(), `swiftc -typecheck ${sentinelPath}`);
  });
});

test('default check-only preflight fails when xcrun is missing or fails', { skip: process.platform === 'win32' }, () => {
  const missingPath = mkdtempSync(path.join(os.tmpdir(), 'petal-no-xcrun-'));
  try {
    const missing = run(preflightPath, ['--check-only'], { PATH: missingPath });
    assert.notEqual(missing.status, 0);
    assert.match(missing.stderr, /spawn(?:Sync)? xcrun ENOENT/);
  } finally {
    rmSync(missingPath, { recursive: true, force: true });
  }
  withFakeXcrun('fail', (env) => {
    const failed = run(preflightPath, ['--check-only'], env);
    assert.notEqual(failed.status, 0);
  });
});

test('loopback forwards the portable skip only when explicitly requested', () => {
  withFakeXcrun('success', (env) => {
    const portable = run(loopbackPath, ['--check-only', '--skip-swift-typecheck'], env);
    assert.equal(portable.status, 0, portable.stderr);
    assertPortableChecks(portable.stdout);
    assert.equal(existsSync(env.tracePath), false, 'portable wrapper path must not invoke xcrun');
  });
  if (process.platform !== 'win32') {
    withFakeXcrun('success', (env) => {
      const defaultPath = run(loopbackPath, ['--check-only'], env);
      assert.equal(defaultPath.status, 0, defaultPath.stderr);
      assert.equal(readFileSync(env.tracePath, 'utf8').trim(), `swiftc -typecheck ${sentinelPath}`);
    });
  }
});

test('skip flag is rejected outside the explicit portable path', () => {
  for (const [script, args] of [
    [preflightPath, ['--skip-swift-typecheck']],
    [loopbackPath, ['--live', '--skip-swift-typecheck']],
    [loopbackPath, ['--check-only', '--skip-preflight']],
    [preflightPath, ['--check-only', '--unknown']],
  ]) {
    const result = run(script, args);
    assert.notEqual(result.status, 0, `${path.basename(script)} ${args.join(' ')} must fail`);
  }
});
