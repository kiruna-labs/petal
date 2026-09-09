// Both-directions test for publish-blob.mjs's feedback-key gate.
//
// This gate previously asserted that a Vite-baked value is findable as
// plaintext in the Mach-O slices. Tauri COMPRESSES the frontend it embeds, so
// that could never be true, and the gate refused two consecutive releases
// (v0.9.12, v0.9.13) on a condition no correct build could satisfy. It was
// written without ever being run against a build that SHOULD pass -- the
// failing direction was the only one anyone had seen.
//
// So this exercises both: a dist that carries the key must be ACCEPTED, and a
// dist that does not must be REFUSED, with and without the expected value
// exported, plus a missing dist directory.
import { mkdtemp, mkdir, writeFile, readFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

const src = await readFile(new URL('./publish-blob.mjs', import.meta.url), 'utf8');
const body = src.slice(src.indexOf('async function verifyFeedbackKey'), src.indexOf('// NEW gate (#874):'));
const scratch = path.join(os.tmpdir(), `feedback-gate-${process.pid}.mjs`);
await writeFile(scratch,
  `import { readFile, readdir, stat } from 'node:fs/promises';\nimport path from 'node:path';\n${body}\nexport { verifyFeedbackKey };`);
const { verifyFeedbackKey } = await import(`file://${scratch}`);

const KEY = 'pk_testkey_abcdefgh12345678';
async function makeDist({ withKey }) {
  const d = await mkdtemp(path.join(os.tmpdir(), 'dist-'));
  await mkdir(path.join(d, '_app', 'immutable'), { recursive: true });
  await writeFile(path.join(d, 'index.html'), '<html>app</html>');
  await writeFile(path.join(d, '_app', 'immutable', 'entry.js'),
    withKey ? `const k=${JSON.stringify(KEY)};export{k};` : 'const k=null;export{k};');
  return d;
}

let failures = 0;
const check = async (label, dir, expectPass) => {
  let err = null;
  try { await verifyFeedbackKey(dir); } catch (e) { err = e; }
  const passed = err === null;
  const ok = passed === expectPass;
  if (!ok) failures++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${label} -> ${passed ? 'ACCEPTED' : 'REFUSED'}${err ? ' :: ' + err.message.slice(0, 120) : ''}`);
};

const withKey = await makeDist({ withKey: true });
const without = await makeDist({ withKey: false });

process.env.VITE_USERDISPATCH_PUBLIC_KEY = KEY;
await check('expected value known, key IS in dist', withKey, true);
await check('expected value known, key NOT in dist', without, false);

delete process.env.VITE_USERDISPATCH_PUBLIC_KEY;
await check('no expected value, a pk_ key IS in dist', withKey, true);
await check('no expected value, no pk_ key in dist', without, false);

process.env.VITE_USERDISPATCH_PUBLIC_KEY = KEY;
await check('dist directory missing entirely', path.join(os.tmpdir(), 'no-such-dist-dir'), false);

console.log(failures === 0 ? '\nALL GATE DIRECTIONS BEHAVE AS INTENDED' : `\n${failures} DIRECTION(S) WRONG`);
process.exit(failures === 0 ? 0 : 1);
