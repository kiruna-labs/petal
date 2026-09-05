import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const draw = readFileSync(new URL('../src-tauri/src/draw.rs', import.meta.url), 'utf8');
const ipc = readFileSync(new URL('../src/lib/ipc.ts', import.meta.url), 'utf8');
const control = readFileSync(new URL('../src/routes/compositor/control/+page.svelte', import.meta.url), 'utf8');
const pointer = readFileSync(new URL('../src/routes/compositor/pointer/+page.svelte', import.meta.url), 'utf8');


test('Draw text stays in the draw protocol and rejects multiline payloads at the native seam', () => {
  assert.match(draw, /DrawMessageType::Text/);
  assert.match(draw, /draw text annotation requires one anchor point/);
  assert.match(draw, /draw text annotation must be single-line/);
  assert.match(draw, /text: message\.text/);
  assert.match(ipc, /'begin' \| 'points' \| 'end' \| 'clear' \| 'text'/);
  assert.match(ipc, /text\?: string;/);
});

test('Draw keyboard input uses a fixed pen anchor and ignores Enter outside composition', () => {
  assert.match(control, /let drawAnchor = \$state<Point \| null>\(null\)/);
  assert.match(control, /if \(drawActive\) \{[\s\S]*?onDrawKey\(event, action\);/);
  assert.match(control, /if \(event\.key === 'Enter'\) return;/);
  assert.match(control, /if \(drawAnchor && drawPointMoved\(drawAnchor, point\)\) commitDrawText\(\);/);
  assert.match(control, /appendDrawText\(event\.data \?\? ''\)/);
  assert.match(control, /requestAnimationFrame\(\(\) => document\.querySelector<HTMLElement>\('\.control-overlay'\)\?\.focus\(\)\)/);
});

test('Committed text renders on the shared overlay and shares the stroke fade clock', () => {
  assert.match(pointer, /interface TrackedTextAnnotation/);
  assert.match(pointer, /if \(update\.type === 'text'\)/);
  assert.match(pointer, /textAnnotationStyle\(annotation\)/);
  assert.match(pointer, /isStrokeExpired\(age\)/);
  assert.match(pointer, /strokeFadeOpacity\(age\)/);
  assert.match(pointer, /textAnnotations = \{\};/);
});
