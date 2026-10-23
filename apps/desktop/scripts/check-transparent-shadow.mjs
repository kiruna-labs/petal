#!/usr/bin/env node
// Lint: every transparent window/panel builder in src-tauri must say
// explicitly whether it has a native window shadow (#406).
//
// AppKit derives a non-opaque window's shadow from its alpha channel, so a
// transparent overlay with the default shadow (Tauri/tao default: on) gets a
// dark outline around whatever web content is visible, including a CSS
// box-shadow's faint footprint. On macOS 26 that reads as a black hairline.
// Overlays and card panels must set `.shadow(false)` / `.has_shadow(false)`;
// a real window that wants the shadow says `.shadow(true)` on purpose.
//
// A builder is the whole statement around a `.transparent(true)` call
// (including `.with_window(|w| ... .transparent(true))` closures inside a
// panel builder chain). Allow-list a statement only with a reason, by putting
// `shadow-lint: allow <reason>` in a comment inside that statement.
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = resolve(fileURLToPath(new URL('../../..', import.meta.url)));
const srcRoot = resolve(repoRoot, 'apps/desktop/src-tauri/src');

function rustFiles(dir) {
  const out = [];
  for (const name of readdirSync(dir)) {
    const abs = join(dir, name);
    if (statSync(abs).isDirectory()) out.push(...rustFiles(abs));
    else if (name.endsWith('.rs')) out.push(abs);
  }
  return out.sort();
}

// Blank out comments and string/char literals (keeping offsets and newlines)
// so brackets and `;` inside them do not confuse the statement scan.
export function maskRust(source) {
  const chars = source.split('');
  let i = 0;
  const blank = (from, to) => {
    for (let k = from; k < to; k += 1) if (chars[k] !== '\n') chars[k] = ' ';
  };
  while (i < source.length) {
    const c = source[i];
    const next = source[i + 1];
    if (c === '/' && next === '/') {
      const end = source.indexOf('\n', i);
      const stop = end === -1 ? source.length : end;
      blank(i, stop);
      i = stop;
    } else if (c === '/' && next === '*') {
      let depth = 1;
      let k = i + 2;
      while (k < source.length && depth > 0) {
        if (source[k] === '/' && source[k + 1] === '*') {
          depth += 1;
          k += 2;
        } else if (source[k] === '*' && source[k + 1] === '/') {
          depth -= 1;
          k += 2;
        } else k += 1;
      }
      blank(i, k);
      i = k;
    } else if (c === 'r' && /^r#*"/.test(source.slice(i, i + 8)) && !/\w/.test(source[i - 1] ?? '')) {
      const hashes = source.slice(i + 1).match(/^#*/)[0];
      const close = `"${hashes}`;
      const start = i + 2 + hashes.length;
      const end = source.indexOf(close, start);
      const stop = end === -1 ? source.length : end + close.length;
      blank(i, stop);
      i = stop;
    } else if (c === '"') {
      let k = i + 1;
      while (k < source.length && source[k] !== '"') k += source[k] === '\\' ? 2 : 1;
      blank(i, k + 1);
      i = k + 1;
    } else if (c === "'" && /^'(\\.|[^\\'\n])'/.test(source.slice(i, i + 4))) {
      const len = source.slice(i, i + 4).match(/^'(\\.|[^\\'\n])'/)[0].length;
      blank(i, i + len);
      i += len;
    } else i += 1;
  }
  return chars.join('');
}

// [start, end) of the statement containing `offset` in masked source.
export function statementBounds(masked, offset) {
  let depth = 0;
  let start = 0;
  for (let k = offset - 1; k >= 0; k -= 1) {
    const c = masked[k];
    if (c === ')' || c === ']' || c === '}') depth += 1;
    else if (c === '(' || c === '[' || c === '{') {
      if (depth > 0) depth -= 1;
      else if (c === '{' && !/\|\s*(move\s*)?$/.test(masked.slice(Math.max(0, k - 200), k))) {
        start = k + 1;
        break;
      }
    } else if (c === ';' && depth === 0) {
      start = k + 1;
      break;
    }
  }
  depth = 0;
  let end = masked.length;
  for (let k = offset; k < masked.length; k += 1) {
    const c = masked[k];
    if (c === '(' || c === '[' || c === '{') depth += 1;
    else if (c === ')' || c === ']' || c === '}') {
      if (depth > 0) depth -= 1;
      else if (c === '}') {
        end = k;
        break;
      }
    } else if (c === ';' && depth === 0) {
      end = k + 1;
      break;
    }
  }
  return [start, end];
}

export function findViolations(source) {
  const masked = maskRust(source);
  const violations = [];
  const seen = new Set();
  const re = /\.\s*transparent\s*\(\s*true\s*\)/g;
  let match;
  while ((match = re.exec(masked)) !== null) {
    const [start, end] = statementBounds(masked, match.index);
    if (seen.has(start)) continue;
    seen.add(start);
    const code = masked.slice(start, end);
    if (/\.\s*(?:has_)?shadow\s*\(\s*(?:true|false)\s*\)/.test(code)) continue;
    if (/shadow-lint:\s*allow\s+\S/.test(source.slice(start, end))) continue;
    const line = source.slice(0, match.index).split('\n').length;
    violations.push({ line, text: source.split('\n')[line - 1].trim() });
  }
  return violations;
}

const isMain = process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  const failures = [];
  for (const abs of rustFiles(srcRoot)) {
    for (const v of findViolations(readFileSync(abs, 'utf8'))) {
      failures.push(`${relative(repoRoot, abs)}:${v.line}: ${v.text}`);
    }
  }
  if (failures.length > 0) {
    console.error(
      [
        'Transparent window builder without an explicit shadow setting.',
        'Add `.shadow(false)` / `.has_shadow(false)` for overlays and card panels (a native shadow on a',
        'transparent window outlines its alpha, see #406), `.shadow(true)` for a real window that wants one,',
        'or a `// shadow-lint: allow <reason>` comment inside the builder statement.',
        ...failures,
      ].join('\n'),
    );
    process.exit(1);
  }
}
