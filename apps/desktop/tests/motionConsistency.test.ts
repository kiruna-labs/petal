import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import {
  baseDuration,
  enterDuration,
  exitDuration,
  feedbackDuration,
  layoutDuration,
  prefersReducedMotion,
  tileLayoutDuration,
  tileTransitionDuration
} from '../src/lib/motion.ts';

const tokensSource = readFileSync(new URL('../../../shared/ui/tokens.css', import.meta.url), 'utf8');
const gallerySource = readFileSync(
  fileURLToPath(new URL('../src/lib/components/Gallery.svelte', import.meta.url)),
  'utf8'
);
const layoutSource = readFileSync(
  fileURLToPath(new URL('../src/routes/+layout.svelte', import.meta.url)),
  'utf8'
);
const modalSource = readFileSync(
  fileURLToPath(new URL('../src/lib/components/Modal.svelte', import.meta.url)),
  'utf8'
);
const windowPickerSource = readFileSync(
  fileURLToPath(new URL('../src/lib/components/WindowPicker.svelte', import.meta.url)),
  'utf8'
);
const networkCockpitSource = readFileSync(
  fileURLToPath(new URL('../src/lib/components/NetworkCockpit.svelte', import.meta.url)),
  'utf8'
);
const remoteHeaderSource = readFileSync(
  fileURLToPath(new URL('../src/lib/components/RemoteWindowHeader.svelte', import.meta.url)),
  'utf8'
);

function withReducedMotion<T>(fn: () => T): T {
  const previous = (globalThis as { window?: unknown }).window;
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value: { matchMedia: () => ({ matches: true }) }
  });
  try {
    return fn();
  } finally {
    if (previous === undefined) delete (globalThis as { window?: unknown }).window;
    else Object.defineProperty(globalThis, 'window', { configurable: true, value: previous });
  }
}

test('semantic motion tokens keep the agreed timing register', () => {
  assert.match(tokensSource, /--motion-feedback:\s*120ms;/);
  assert.match(tokensSource, /--motion-exit:\s*120ms;/);
  assert.match(tokensSource, /--motion-enter:\s*180ms;/);
  assert.match(tokensSource, /--motion-layout:\s*220ms;/);
  assert.match(tokensSource, /--motion-distance:\s*4px;/);
  assert.match(tokensSource, /--motion-tooltip-delay:\s*550ms;/);
  assert.match(tokensSource, /--ease-exit:\s*cubic-bezier\(0\.4,\s*0,\s*1,\s*1\);/);
  assert.match(tokensSource, /--motion-fast:\s*var\(--motion-feedback\);/);
  assert.match(tokensSource, /--motion-base:\s*var\(--motion-enter\);/);
  assert.match(
    tokensSource,
    /--motion-feedback:\s*0ms;[\s\S]*--motion-exit:\s*0ms;[\s\S]*--motion-enter:\s*0ms;[\s\S]*--motion-layout:\s*0ms;[\s\S]*--motion-distance:\s*0px;[\s\S]*--motion-tooltip-delay:\s*0ms;/
  );
});

test('Svelte motion helpers mirror semantic tokens and reduced motion', () => {
  assert.equal(prefersReducedMotion(), false);
  assert.equal(feedbackDuration(), 120);
  assert.equal(exitDuration(), 120);
  assert.equal(enterDuration(), 180);
  assert.equal(baseDuration(), 180);
  assert.equal(layoutDuration(), 220);
  assert.equal(tileTransitionDuration(), 180);
  assert.equal(tileLayoutDuration(), 220);

  withReducedMotion(() => {
    assert.equal(prefersReducedMotion(), true);
    assert.equal(feedbackDuration(), 0);
    assert.equal(exitDuration(), 0);
    assert.equal(enterDuration(), 0);
    assert.equal(layoutDuration(), 0);
    assert.equal(tileTransitionDuration(), 0);
    assert.equal(tileLayoutDuration(), 0);
  });
});

test('route and gallery motion avoid layout-property animation', () => {
  assert.doesNotMatch(layoutSource, /(?:animation|transition)[^;{}]*220ms/);
  assert.match(layoutSource, /animation: petal-route-(?:in|out) var\(--motion-enter\)/);
  assert.match(layoutSource, /translateY\(var\(--motion-distance\)\)/);
  assert.doesNotMatch(gallerySource, /(?:width|height) var\(--motion-/);
  assert.match(gallerySource, /transition-delay: var\(--motion-tooltip-delay\);/);
  assert.match(gallerySource, /function transitionGalleryLayout\(mutate: \(\) => void\)/);
  assert.match(gallerySource, /tile\.animate\(/);
  assert.match(gallerySource, /duration, easing: 'cubic-bezier\(0\.2, 0, 0, 1\)', fill: 'none'/);
  assert.match(gallerySource, /animate:flip=\{\{ duration: suppressSvelteFlip \? 0 : tileLayoutDuration\(\) \}\}/);
  assert.match(gallerySource, /suppressSvelteFlip = duration > 0/);
  assert.doesNotMatch(modalSource, /\b\d+ms\b/);
  assert.doesNotMatch(windowPickerSource, /animation-delay:\s*\d+ms/);
  assert.doesNotMatch(networkCockpitSource, /animation-delay:\s*\d+ms/);
  assert.doesNotMatch(remoteHeaderSource, /transition-duration:\s*(?:160|240)ms/);
  assert.doesNotMatch(remoteHeaderSource, /transition-property:\s*[^;]*\bwidth\b/);
});
