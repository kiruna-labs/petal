#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = resolve(fileURLToPath(new URL('../../..', import.meta.url)));
const files = [
  'apps/desktop/src-tauri/src/compositor.rs',
  'apps/desktop/src-tauri/src/share_border.rs',
  'apps/desktop/src-tauri/src/share_notice.rs',
  'apps/desktop/src-tauri/src/control_consent.rs',
];

function stripCommentsLineAware(source) {
  let inBlock = false;
  return source.split(/\r?\n/).map((line) => {
    let out = '';
    for (let i = 0; i < line.length; i += 1) {
      if (inBlock) {
        if (line[i] === '*' && line[i + 1] === '/') {
          inBlock = false;
          i += 1;
        }
        continue;
      }
      if (line[i] === '/' && line[i + 1] === '*') {
        inBlock = true;
        i += 1;
        continue;
      }
      if (line[i] === '/' && line[i + 1] === '/') {
        break;
      }
      out += line[i];
    }
    return out;
  });
}

const failures = [];
for (const file of files) {
  const abs = resolve(repoRoot, file);
  const lines = stripCommentsLineAware(readFileSync(abs, 'utf8'));
  lines.forEach((line, index) => {
    if (/\.\s*close\s*\(/.test(line)) {
      failures.push(`${relative(repoRoot, abs)}:${index + 1}: ${line.trim()}`);
    }
  });
}

if (failures.length > 0) {
  console.error(
    [
      'Forbidden panel close call found.',
      'tauri_nspanel teardown must hide + retire + reuse panels; window.close() has caused SIGABRT during deferred AppKit teardown.',
      ...failures,
    ].join('\n'),
  );
  process.exit(1);
}
