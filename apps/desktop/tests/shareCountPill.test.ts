import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import {
  SHARE_COUNT_PILL_CAP,
  SHARE_COUNT_PILL_MIN,
  localShareCountPillAriaLabel,
  shareCountPillAriaLabel,
  shareCountPillLabel,
  shouldShowSharePill
} from '../src/lib/data/shareCountPill.ts';
import { COMMANDS } from '../src/lib/ipc.ts';

test('#875: pill renders whenever the participant is sharing (>= 1)', () => {
  // Revised 2026-08-26: was `>= 2`. Gating at 2 left a single-window sharer
  // with NO clickable affordance, so "click their portrait to raise their
  // windows" did not exist in the most common case -- the owner's report.
  // A non-sharer (0) must still show nothing.
  assert.equal(SHARE_COUNT_PILL_MIN, 1);
  assert.equal(shouldShowSharePill(0), false);
  assert.equal(shouldShowSharePill(1), true);
  assert.equal(shouldShowSharePill(2), true);
  assert.equal(shouldShowSharePill(9), true);
  assert.equal(shouldShowSharePill(50), true);
});

test('#875: displayed count caps at "9+", never a clipped larger number', () => {
  assert.equal(SHARE_COUNT_PILL_CAP, 9);
  assert.equal(shareCountPillLabel(2), '2');
  assert.equal(shareCountPillLabel(9), '9');
  assert.equal(shareCountPillLabel(10), '9+');
  assert.equal(shareCountPillLabel(42), '9+');
  // Never a truncated "1" for 10+, "4" for 42, etc. -- always the full "9+" glyph.
  assert.notEqual(shareCountPillLabel(10), '1');
  assert.notEqual(shareCountPillLabel(42), '4');
});

test('#875: aria-label states the REAL count, not the capped display label', () => {
  assert.equal(shareCountPillAriaLabel(2, 'Bob'), '2 windows shared by Bob — bring to front');
  assert.equal(shareCountPillAriaLabel(1, 'Bob'), '1 window shared by Bob — bring to front');
  // A 12-window sharer's pill shows "9+", but the aria-label still says 12.
  assert.equal(shareCountPillAriaLabel(12, 'Ada'), '12 windows shared by Ada — bring to front');
  assert.doesNotMatch(shareCountPillAriaLabel(12, 'Ada'), /9\+/);
});

test('#875: local (non-interactive) aria-label carries no "bring to front" action language', () => {
  assert.equal(localShareCountPillAriaLabel(3), 'Sharing 3 windows');
  assert.equal(localShareCountPillAriaLabel(1), 'Sharing 1 window');
  assert.doesNotMatch(localShareCountPillAriaLabel(3), /bring to front/);
});

test('#875: compositor_raise_participant_windows is registered in the IPC command table', () => {
  const ipc = readFileSync(new URL('../src/lib/ipc.ts', import.meta.url), 'utf8');
  assert.equal(COMMANDS.compositorRaiseParticipantWindows, 'compositor_raise_participant_windows');
  assert.match(
    ipc,
    /compositorRaiseParticipantWindows: 'compositor_raise_participant_windows'/
  );
  assert.match(
    ipc,
    /\[COMMANDS\.compositorRaiseParticipantWindows\]: \{ ownerIdentity: string \}/
  );
});

test('#875: ParticipantTile wires the pill to real props, a gated <button> for remote, and a plain span for local', () => {
  const tile = readFileSync(
    new URL('../src/lib/components/ParticipantTile.svelte', import.meta.url),
    'utf8'
  );

  // Real props, not hardcoded demo values.
  assert.match(tile, /shareCount\s*=\s*0/);
  assert.match(tile, /sharingLiveBackground/);
  assert.match(tile, /sharingLiveColor/);
  assert.match(tile, /isLocal\s*=\s*false/);

  // Gating uses the pure helper, not ad-hoc inline logic that could drift
  // from the tested behavior above.
  assert.match(tile, /const showSharePill = \$derived\(shouldShowSharePill\(shareCount\)\);/);
  assert.match(tile, /const sharePillLabel = \$derived\(shareCountPillLabel\(shareCount\)\);/);

  // Local tile: plain, non-interactive <span> -- no button semantics.
  assert.match(tile, /\{#if isLocal\}\s*<span\s+class="share-count-pill"/);

  // Remote tile: real <button>, stops propagation (the tile wrapper is
  // itself role="button" with its own pin onclick in Gallery.svelte), and
  // invokes the real command with ownerIdentity.
  assert.match(tile, /<button[\s\S]*?class="share-count-pill interactive"[\s\S]*?onclick=\{handleRaiseParticipantWindows\}/);
  assert.match(tile, /function handleRaiseParticipantWindows\(event: MouseEvent\) \{/);
  const handlerStart = tile.indexOf('function handleRaiseParticipantWindows(event: MouseEvent) {');
  const handlerEnd = tile.indexOf('\n  }', handlerStart);
  const handlerBody = tile.slice(handlerStart, handlerEnd);
  assert.match(handlerBody, /event\.stopPropagation\(\);/);
  assert.match(
    handlerBody,
    /invoke\(COMMANDS\.compositorRaiseParticipantWindows, \{ ownerIdentity \}\)/
  );
});

test('#875: Gallery passes shareCount/sharing colors/isLocal through its single keyed tile call site', () => {
  const gallery = readFileSync(
    new URL('../src/lib/components/Gallery.svelte', import.meta.url),
    'utf8'
  );

  // The keyed participant tree has one ParticipantTile call site, so these
  // props cannot drift between separate grid and spotlight branches.
  const shareCountSites = [...gallery.matchAll(/shareCount=\{p\.shareCount \?\? 0\}/g)];
  assert.equal(shareCountSites.length, 1, 'expected one shareCount prop on the keyed tile call site');

  const isLocalSites = [...gallery.matchAll(/isLocal=\{p\.isLocal \?\? false\}/g)];
  assert.equal(isLocalSites.length, 1, 'expected one isLocal prop on the keyed tile call site');

  assert.match(gallery, /shareCount\?: number;/);
  assert.match(gallery, /sharingLiveBackground\?: string;/);
  assert.match(gallery, /sharingLiveColor\?: string;/);

  // Hidden entirely at `tiny` -- drop, never shrink past legibility (the
  // muted-chip precedent this file already follows).
  assert.match(
    gallery,
    /\.tiles\.grid\.tiny \.tile-wrap :global\(\.share-count-pill\) \{\s*display:\s*none;/
  );
  // Shrunk (not hidden) at the compact/spotlight-thumb breakpoint, mirroring
  // the compact chip rules.
  assert.match(gallery, /\.tiles\.grid\.compact \.tile-wrap :global\(\.share-count-pill\)/);
});

test('#875: Filmstrip hides the pill entirely at its fixed small tile size', () => {
  const filmstrip = readFileSync(
    new URL('../src/lib/components/Filmstrip.svelte', import.meta.url),
    'utf8'
  );
  assert.match(filmstrip, /shareCount=\{p\.shareCount \?\? 0\}/);
  assert.match(
    filmstrip,
    /\.slot :global\(\.share-count-pill\) \{\s*display:\s*none;/
  );
});

test('#875: galleryBridge emits windowShareCounts alongside sharingIdentities', () => {
  const bridge = readFileSync(
    new URL('../src/lib/data/galleryBridge.ts', import.meta.url),
    'utf8'
  );
  assert.match(bridge, /windowShareCounts: Record<string, number>;/);
  assert.match(
    bridge,
    /windowShareCounts: Object\.fromEntries\(\s*\[\.\.\.windowShares\.entries\(\)\]/
  );
});

test('#875: meetingSession consumes windowShareCounts and exposes a local shareCount channel', () => {
  const session = readFileSync(
    new URL('../src/lib/meeting/meetingSession.svelte.ts', import.meta.url),
    'utf8'
  );
  assert.match(session, /localShareCount: \(\) => number;/);
  assert.match(session, /windowShareCounts = signals\.windowShareCounts;/);
  assert.match(
    session,
    /const shareCount = p\.isLocal \? localShareCount : \(windowShareCounts\[p\.identity\] \?\? 0\);/
  );
});

test('#875: the meeting route derives localShareCount from the same sharedWindowIds command backing shareActive', () => {
  const route = readFileSync(
    new URL('../src/routes/meeting/[room]/+page.svelte', import.meta.url),
    'utf8'
  );
  assert.match(route, /let shareCount = \$state\(0\);/);
  assert.match(route, /localShareCount: \(\) => shareCount,/);
  const refreshStart = route.indexOf('async function refreshShareState() {');
  const refreshEnd = route.indexOf('\n  }', refreshStart);
  const refreshBody = route.slice(refreshStart, refreshEnd);
  assert.match(refreshBody, /const ids = await invoke<number\[\]>\(COMMANDS\.sharedWindowIds\);/);
  assert.match(refreshBody, /shareActive = ids\.length > 0;/);
  assert.match(refreshBody, /shareCount = ids\.length;/);
});
