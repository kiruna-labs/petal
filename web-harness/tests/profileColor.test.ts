import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { HARNESS_COLOR_STORAGE_KEY } from '../src/constants.ts';
import { shouldShowFirstVisitOnboarding, HARNESS_IDENTITY_STORAGE_KEY } from '../src/controls.ts';
import {
  ensureStoredColorIndex,
  loadStoredColorIndex,
  nextProfileColorIndex,
  parseStoredColorIndex,
  saveStoredColorIndex,
} from '../src/homeScreen.ts';
import {
  identityPaletteIndexFromMetadata,
  mergeIdentityPaletteIndexMetadata,
  sharedSourceKindFromMetadata,
} from '../src/trackNames.ts';
import {
  colorForIdentity,
  IDENTITY_COLOR_PALETTE,
  identityHeaderCss,
  inkForIdentity,
} from '../src/telepointer.ts';

const contractFixture = JSON.parse(
  readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8'),
) as {
  identityPalette: {
    hash: string;
    names: string[];
    hex: string[];
  };
  identityPaletteMetadata: {
    metadata: string;
    paletteIndex: number;
    windowId: number;
  };
};

class MemoryStorage implements Pick<Storage, 'getItem' | 'setItem'> {
  readonly values = new Map<string, string>();

  getItem(key: string): string | null {
    return this.values.get(key) ?? null;
  }

  setItem(key: string, value: string): void {
    this.values.set(key, value);
  }
}

test('profile color selection persists as a local palette index', () => {
  const storage = new MemoryStorage();

  saveStoredColorIndex(storage, 4);

  assert.equal(storage.getItem(HARNESS_COLOR_STORAGE_KEY), '4');
  assert.equal(loadStoredColorIndex(storage), 4);
  assert.equal(parseStoredColorIndex('6'), null);
  assert.equal(parseStoredColorIndex('plum'), null);
});

test('compact profile color palette supports predictable two-column keyboard movement', () => {
  assert.equal(nextProfileColorIndex(0, 'ArrowRight'), 1);
  assert.equal(nextProfileColorIndex(0, 'ArrowLeft'), 5);
  assert.equal(nextProfileColorIndex(0, 'ArrowDown'), 2);
  assert.equal(nextProfileColorIndex(3, 'ArrowUp'), 1);
  assert.equal(nextProfileColorIndex(5, 'ArrowRight'), 0);
  assert.equal(nextProfileColorIndex(3, 'Home'), 0);
  assert.equal(nextProfileColorIndex(3, 'End'), 5);
  assert.equal(nextProfileColorIndex(3, 'Escape'), null);
});

test('ensureStoredColorIndex preserves a valid stored color', () => {
  const storage = new MemoryStorage();
  storage.setItem(HARNESS_COLOR_STORAGE_KEY, '4');

  assert.equal(ensureStoredColorIndex(storage), 4);
  assert.equal(storage.getItem(HARNESS_COLOR_STORAGE_KEY), '4');
});

test('ensureStoredColorIndex randomly assigns and persists a color when none is stored', () => {
  const storage = new MemoryStorage();

  // fixed rng => deterministic index, proving the rng actually drives the pick
  // rather than always landing on a hardcoded value
  const index = ensureStoredColorIndex(storage, IDENTITY_COLOR_PALETTE.length, () => 0.6);

  assert.equal(index, Math.floor(0.6 * IDENTITY_COLOR_PALETTE.length));
  assert.equal(storage.getItem(HARNESS_COLOR_STORAGE_KEY), String(index));
});

test('ensureStoredColorIndex draws from the full palette range, not a fixed value', () => {
  const seen = new Set<number>();
  for (let i = 0; i < IDENTITY_COLOR_PALETTE.length; i++) {
    const storage = new MemoryStorage();
    const rng = () => i / IDENTITY_COLOR_PALETTE.length;
    seen.add(ensureStoredColorIndex(storage, IDENTITY_COLOR_PALETTE.length, rng));
  }
  assert.equal(seen.size, IDENTITY_COLOR_PALETTE.length);
});

test('ensureStoredColorIndex repairs an invalid stored color by randomly reassigning', () => {
  const storage = new MemoryStorage();
  storage.setItem(HARNESS_COLOR_STORAGE_KEY, 'not-a-color');

  const index = ensureStoredColorIndex(storage, IDENTITY_COLOR_PALETTE.length, () => 0.1);

  assert.equal(index, Math.floor(0.1 * IDENTITY_COLOR_PALETTE.length));
  assert.equal(storage.getItem(HARNESS_COLOR_STORAGE_KEY), String(index));
});

test('ensureStoredColorIndex never calls the rng when a valid color is already stored', () => {
  const storage = new MemoryStorage();
  storage.setItem(HARNESS_COLOR_STORAGE_KEY, '2');
  let rngCalls = 0;

  assert.equal(
    ensureStoredColorIndex(storage, IDENTITY_COLOR_PALETTE.length, () => {
      rngCalls += 1;
      return 0;
    }),
    2,
  );
  assert.equal(rngCalls, 0);
});

test('colorForIdentity honors an override and otherwise hashes into the native palette', () => {
  assert.equal(colorForIdentity('web-alpha', 1), '#6e8bff');
  assert.equal(inkForIdentity('web-alpha', 1), '#081129');
  assert.equal(colorForIdentity('web-alpha'), IDENTITY_COLOR_PALETTE[1]);
  assert.equal(colorForIdentity('native-user-42'), IDENTITY_COLOR_PALETTE[4]);
});

test('identityHeaderCss pairs the hashed palette color with its ink', () => {
  const header = identityHeaderCss('native-1');
  assert.equal(header.background, colorForIdentity('native-1'));
  assert.equal(header.ink, inkForIdentity('native-1'));
});

test('web identity palette hexes match the desktop identityColor.ts palette', () => {
  const source = readFileSync(
    new URL('../../apps/desktop/src/lib/data/identityColor.ts', import.meta.url),
    'utf8'
  );
  const hexBlock = source.match(/IDENTITY_COLOR_HEX[\s\S]*?};/)?.[0] ?? '';
  const matches = Array.from(hexBlock.matchAll(/(plum|blue|green|amber|lilac|slate): '(#[0-9a-f]{6})'/g));
  const nativeHexes = matches.map((match) => match[2]);

  assert.deepEqual(nativeHexes, [...IDENTITY_COLOR_PALETTE]);
});

test('web identity palette hexes match the shared contract fixture', () => {
  assert.equal(contractFixture.identityPalette.hash, 'utf16-hash-times-31-mod-6');
  assert.deepEqual(contractFixture.identityPalette.hex, [...IDENTITY_COLOR_PALETTE]);
});

test('participant identity palette metadata round-trips and preserves window metadata', () => {
  const merged = mergeIdentityPaletteIndexMetadata('{"petalWindowKinds":{"42":"display"}}', 2);

  assert.equal(identityPaletteIndexFromMetadata(merged), 2);
  assert.equal(sharedSourceKindFromMetadata(merged, 42), 'display');
  assert.equal(identityPaletteIndexFromMetadata(mergeIdentityPaletteIndexMetadata(merged, 99)), null);
  assert.equal(identityPaletteIndexFromMetadata(contractFixture.identityPaletteMetadata.metadata), 2);
  assert.equal(
    sharedSourceKindFromMetadata(
      contractFixture.identityPaletteMetadata.metadata,
      contractFixture.identityPaletteMetadata.windowId,
    ),
    'display',
  );
});

test('first-visit onboarding is shown only before identity or color exists', () => {
  const storage = new MemoryStorage();

  assert.equal(shouldShowFirstVisitOnboarding(storage), true);
  storage.setItem(HARNESS_COLOR_STORAGE_KEY, '0');
  assert.equal(shouldShowFirstVisitOnboarding(storage), false);

  const identityOnly = new MemoryStorage();
  identityOnly.setItem(HARNESS_IDENTITY_STORAGE_KEY, 'web-existing');
  assert.equal(shouldShowFirstVisitOnboarding(identityOnly), false);
});
