#!/usr/bin/env node
// Test Cockpit Phase 3 dev wrapper (#257).
//
// This script intentionally contains no scenario engine. It launches the
// QA-featured desktop binary with `--test-case=<selector>`, which routes into
// the same in-process Rust `test_cockpit` engine used by the Settings entry
// point and launch-param CI path.

import { execFileSync, spawn } from 'node:child_process';
import { existsSync, statSync } from 'node:fs';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

const SCRIPT_DIR = path.dirname(fileURLToPath(import.meta.url));
const APP_DIR = path.resolve(SCRIPT_DIR, '..');
const TAURI_DIR = path.join(APP_DIR, 'src-tauri');
const FRONTEND_DIST = path.join(APP_DIR, 'build');
const REQUIRED_COCKPIT_ASSETS = [
  'dev/test-pattern.html',
  'dev/test-pattern-status.html',
];

// Cargo's direct QA launcher can otherwise reuse a stale embedded frontend
// from an earlier build. Fail before starting any windows when the two cockpit
// pages are absent, and print reproducible provenance for the run artifact/log.
function assertCockpitFrontendAssets() {
  const missing = REQUIRED_COCKPIT_ASSETS.filter(
    (relative) => !existsSync(path.join(FRONTEND_DIST, relative))
  );
  if (missing.length > 0) {
    throw new Error(
      `test-cockpit: frontend build is missing ${missing.join(', ')}. Run npm run build in apps/desktop before launching the cockpit.`
    );
  }
  let gitHead = 'unavailable';
  try {
    gitHead = execFileSync('git', ['rev-parse', 'HEAD'], {
      cwd: APP_DIR,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
    }).trim();
  } catch {
    // Source archives are valid inputs too; the absolute artifact path and
    // mtimes still identify exactly what was checked.
  }
  const assets = REQUIRED_COCKPIT_ASSETS.map((relative) => {
    const absolute = path.join(FRONTEND_DIST, relative);
    return { relative, mtimeMs: Math.trunc(statSync(absolute).mtimeMs) };
  });
  console.log(
    `test-cockpit: frontend preflight OK gitHead=${gitHead} dist=${FRONTEND_DIST} assets=${JSON.stringify(assets)}`
  );
}

function usage() {
  return [
    'Usage: node apps/desktop/scripts/cockpit.mjs [SELECTOR]',
    '',
    'Selectors (docs/TEST_PLAN.md is the authority; all case-insensitive):',
    '  a phase        get-in|join|speak|see|share|control|point|survive|look',
    '  a journey id   AUD-01, SHARE-01, ...',
    '  a feature      audio, screen-sharing, A..I',
    '  priority/depth p0|p1|p2, short|long',
    '  a direction    web-nat|nat-web|nat-nat|nat-local|web-local',
    '  intersection   speak:web-nat  (audio, one way)   p0:short',
    '  a comma list   AUD-01,CAM-01',
    '  legacy tiers   quick|full|soak',
    '',
    'Env:',
    '  PETAL_BACKEND_URL     Backend target, default handled by the Rust app',
    '  PETAL_HARNESS_URL     Web harness target, default https://meet.petal.live',
    '  PETAL_CHROME_BIN      Chrome executable for release-path web peer launch',
    '  PETAL_DISABLE_AUDIO   Defaults to 1 (video-only). Audio journeys need 0.',
  ].join('\n');
}

// An audio journey run with PETAL_DISABLE_AUDIO=1 skips the mic publish and
// speaker playout it exists to verify. The AUD scenario's decoded-PCM oracle
// still exercises the receive decode, but a video-only run must never be
// mistaken for full audio validation (the exact confusion #787 grew in), so
// refuse the combination instead of annotating it.
function selectorWantsAudio(selector) {
  return /(^|[,:])(speak|aud[-a-z0-9]*|audio|c)($|[,:])/i.test(selector.trim());
}

function selectorFromArgs(argv) {
  let selector = null;
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '-h' || arg === '--help') {
      console.log(usage());
      process.exit(0);
    }
    if (arg.startsWith('--test-case=')) {
      selector = arg.slice('--test-case='.length);
      continue;
    }
    if (arg === '--test-case') {
      selector = argv[i + 1] ?? '';
      i += 1;
      continue;
    }
    if (!arg.startsWith('-') && selector === null) {
      selector = arg;
      continue;
    }
    throw new Error(`unknown argument: ${arg}\n\n${usage()}`);
  }
  return (selector ?? process.env.PETAL_TEST_CASE ?? 'quick').trim();
}

function runCargo(selector) {
  return new Promise((resolve, reject) => {
    const child = spawn(
      'cargo',
      ['run', '--features', 'cockpit-privileged', '--', `--test-case=${selector}`],
      {
        cwd: TAURI_DIR,
        env: {
          ...process.env,
          PETAL_DISABLE_AUDIO: process.env.PETAL_DISABLE_AUDIO ?? '1',
          DEVELOPER_DIR: process.env.DEVELOPER_DIR ?? '/Library/Developer/CommandLineTools',
        },
        stdio: 'inherit',
      }
    );
    child.on('error', reject);
    child.on('close', (code, signal) => resolve({ code, signal }));
  });
}

try {
  const selector = selectorFromArgs(process.argv.slice(2));
  if (!selector) throw new Error(`test case selector is empty\n\n${usage()}`);
  if (selectorWantsAudio(selector) && (process.env.PETAL_DISABLE_AUDIO ?? '1') !== '0') {
    throw new Error(
      `selector '${selector}' targets audio but PETAL_DISABLE_AUDIO is not 0 -- ` +
        'a video-only run cannot validate audio (#787). Re-run with PETAL_DISABLE_AUDIO=0.'
    );
  }
  assertCockpitFrontendAssets();
  console.log(`test-cockpit: delegating to Rust engine selector=${selector}`);
  const { code, signal } = await runCargo(selector);
  if (signal) {
    console.error(`test-cockpit: cargo run terminated by ${signal}`);
    process.exitCode = 1;
  } else {
    process.exitCode = code ?? 1;
  }
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
}
