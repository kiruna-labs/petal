#!/usr/bin/env node
// #416 pre-flight: WHERE does a posted mouse-down have to land to actuate a
// receiver remote window's resize handle?
//
// The previous live-validation attempt aborted with NO RESULT because its
// positive control -- a plain drag with no source change -- never moved the
// panel, so nothing measured after it could mean anything. It could not tell
// "the handle rect is somewhere else" from "posted pointer sequences never
// reach `.resize-zone` at all". This sweep answers exactly that question and
// nothing else: it enumerates the receiver's real window stack, then tries a
// short drag at each candidate handle point and reports which (if any)
// actually changed the panel's WindowServer frame.
//
// Run it before acceptance-416.mjs. If every candidate is dead, the honest
// output is "the harness cannot actuate the handle", not a row of zeros.

import { execFileSync, spawnSync } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const probeSource = path.join(scriptDir, 'petal-window-probe.swift');
const probeBinary = path.join(os.tmpdir(), 'petal-acceptance-416-probe');

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function compileProbe() {
  const result = spawnSync('xcrun', ['swiftc', probeSource, '-framework', 'AppKit', '-o', probeBinary], {
    encoding: 'utf8',
    env: {
      ...process.env,
      SWIFT_MODULE_CACHE_PATH: path.join(os.tmpdir(), 'petal-416-swift-cache'),
      CLANG_MODULE_CACHE_PATH: path.join(os.tmpdir(), 'petal-416-clang-cache'),
    },
  });
  if (result.status !== 0) throw new Error(`probe compile failed: ${(result.stderr || result.stdout || '').trim()}`);
}

function probe(args) {
  const out = execFileSync(probeBinary, args, { encoding: 'utf8', timeout: 30000 });
  return out.trim().split('\n').filter(Boolean).map((line) => JSON.parse(line));
}

const PETAL_OWNER = /^(petal|desktop)$/i;
function findWindows() {
  return (probe(['--find'])[0] ?? []).filter((w) => PETAL_OWNER.test(w.owner));
}

function frameOf(windowNumber) {
  return findWindows().find((w) => w.windowNumber === windowNumber) ?? null;
}

compileProbe();

const all = findWindows();
console.log('# receiver window stack (owner petal/desktop), z ascending = front to back:');
for (const w of all) {
  console.log(
    `#   z=${String(w.z).padStart(3)} layer=${w.layer} num=${w.windowNumber} ` +
      `${String(w.w).padStart(5)}x${String(w.h).padStart(5)} at (${w.x},${w.y}) name='${w.name}'`
  );
}

const panel = all.find((w) => /^petal-window-\d+$/.test(w.name) && w.layer === 0 && w.w > 200);
if (!panel) {
  console.log('SWEEP416 no receiver remote window on screen -- bring the two-peer loop up first');
  process.exit(2);
}
// Overlay children carry `remote-window-{control,pointer}-<seg>-<id>` names and
// cover only the video area below the 44pt header strip.
const control = all.find((w) => /^remote-window-control-/.test(w.name));
const HEADER = 44;

// Candidate handle points, each with the CSS rect it is aimed at.
const candidates = [
  {
    id: 'control-east-mid',
    note: "control overlay `.resize-e`: right:0 width:14, top:0 bottom:22 of the video area",
    point: () => {
      const f = control ?? { x: panel.x, y: panel.y + HEADER, w: panel.w, h: panel.h - HEADER };
      return { x: f.x + f.w - 7, y: f.y + Math.round((f.h - 22) / 2) };
    },
  },
  {
    id: 'panel-header-east',
    note: 'surface `.resize-e`: right:0 width:12, but only inside the 44pt header strip',
    point: () => ({ x: panel.x + panel.w - 6, y: panel.y + 27 }),
  },
  {
    id: 'control-se-grip',
    note: 'control overlay `.resize-se` corner grip',
    point: () => {
      const f = control ?? { x: panel.x, y: panel.y + HEADER, w: panel.w, h: panel.h - HEADER };
      return { x: f.x + f.w - 8, y: f.y + f.h - 8 };
    },
  },
  {
    id: 'panel-ne-grip',
    note: 'surface `.resize-ne`: 28x28 at the panel top-right corner',
    point: () => ({ x: panel.x + panel.w - 12, y: panel.y + 12 }),
  },
  {
    id: 'panel-east-mid-attempt2',
    note: 'the point the previous attempt used: 6pt in from the right edge, at panel mid-height',
    point: () => ({ x: panel.x + panel.w - 6, y: panel.y + Math.round(panel.h / 2) }),
  },
];

async function tryCandidate(candidate, dx) {
  const startFrame = frameOf(panel.windowNumber);
  if (!startFrame) return { id: candidate.id, error: 'panel vanished' };
  // Re-read geometry for EVERY candidate. A candidate that actuates changes
  // the panel's frame, so reusing the frame captured at startup aims every
  // later candidate at a point that is no longer on the window -- which reads
  // as "that handle does not work" when nothing was ever clicked.
  panel.x = startFrame.x;
  panel.y = startFrame.y;
  panel.w = startFrame.w;
  panel.h = startFrame.h;
  const live = findWindows().find((w) => /^remote-window-control-/.test(w.name));
  if (live && control) Object.assign(control, live);
  probe(['--activate', String(panel.pid)]);
  await sleep(400);
  const p = candidate.point();
  probe(['--hover', String(p.x - 24), String(p.y)]);
  await sleep(60);
  probe(['--hover', String(p.x), String(p.y)]);
  await sleep(120);
  probe(['--press', String(p.x), String(p.y)]);
  await sleep(150);
  for (let step = 1; step <= 8; step += 1) {
    probe(['--move', String(Math.round(p.x + (dx * step) / 8)), String(p.y)]);
    await sleep(45);
  }
  probe(['--release', String(Math.round(p.x + dx)), String(p.y)]);
  await sleep(700);
  const endFrame = frameOf(panel.windowNumber);
  return {
    id: candidate.id,
    point: p,
    from: `${startFrame.w}x${startFrame.h}@(${startFrame.x},${startFrame.y})`,
    to: endFrame ? `${endFrame.w}x${endFrame.h}@(${endFrame.x},${endFrame.y})` : 'gone',
    dw: endFrame ? endFrame.w - startFrame.w : null,
    dh: endFrame ? endFrame.h - startFrame.h : null,
    dx: endFrame ? endFrame.x - startFrame.x : null,
    actuated: endFrame ? Math.abs(endFrame.w - startFrame.w) >= 20 || Math.abs(endFrame.h - startFrame.h) >= 20 : false,
  };
}

console.log(`# panel #${panel.windowNumber} pid=${panel.pid} ${panel.w}x${panel.h} at (${panel.x},${panel.y})`);
console.log(control ? `# control overlay ${control.w}x${control.h} at (${control.x},${control.y})` : '# NO control overlay window found');

const findings = [];
for (const candidate of candidates) {
  // Alternate shrink/grow so a run that hits a min-size clamp still shows
  // movement on the next candidate.
  const result = await tryCandidate(candidate, findings.length % 2 === 0 ? -120 : 120);
  findings.push(result);
  console.log(`SWEEP416 ${JSON.stringify(result)}`);
  await sleep(400);
}

const winners = findings.filter((f) => f.actuated);
console.log(`SWEEP416_SUMMARY ${JSON.stringify({ actuated: winners.map((w) => w.id), tried: findings.length })}`);
process.exitCode = winners.length ? 0 : 2;
