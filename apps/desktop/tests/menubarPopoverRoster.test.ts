import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const rosterPopover = readFileSync(
  new URL('../src/lib/components/RosterPopover.svelte', import.meta.url),
  'utf8'
);
const menubarPopover = readFileSync(
  new URL('../src/routes/menubar-popover/+page.svelte', import.meta.url),
  'utf8'
);
const devMenubarPopover = readFileSync(
  new URL('../src/routes/dev/menubar-popover/+page.svelte', import.meta.url),
  'utf8'
);

test('menubar popover embeds the roster instead of stacking a separate roster card', () => {
  assert.match(menubarPopover, /<RosterPopover[\s\S]*embedded/);
  assert.match(devMenubarPopover, /<RosterPopover[\s\S]*embedded/);
  assert.match(rosterPopover, /embedded\?: boolean/);
  assert.match(rosterPopover, /<div class="roster-popover" class:embedded>/);
});

test('embedded roster removes standalone modal chrome but keeps default standalone card', () => {
  assert.match(rosterPopover, /\.roster-popover\s*{[\s\S]*width:\s*260px;/);
  assert.match(rosterPopover, /\.roster-popover\s*{[\s\S]*box-shadow:\s*var\(--shadow-panel/);
  assert.match(rosterPopover, /\.roster-popover\.embedded\s*{[\s\S]*width:\s*100%;/);
  assert.match(rosterPopover, /\.roster-popover\.embedded\s*{[\s\S]*border-radius:\s*0;/);
  assert.match(rosterPopover, /\.roster-popover\.embedded\s*{[\s\S]*background:\s*transparent;/);
  assert.match(rosterPopover, /\.roster-popover\.embedded\s*{[\s\S]*box-shadow:\s*none;/);
});

test('menubar popover host owns the unified card chrome', () => {
  assert.match(menubarPopover, /\.menubar-popover-host\s*{[\s\S]*width:\s*280px;/);
  assert.match(menubarPopover, /\.menubar-popover-host\s*{[\s\S]*border-radius:\s*var\(--radius-card\);/);
  assert.match(
    menubarPopover,
    /\.menubar-popover-host\s*{[\s\S]*background:\s*linear-gradient\(180deg,\s*var\(--surface-raised\),\s*var\(--surface\)\);/
  );
  assert.match(menubarPopover, /\.menubar-popover-host\s*{[\s\S]*border:\s*1px solid var\(--hairline\);/);
  assert.match(menubarPopover, /\.menubar-popover-host\s*{[\s\S]*box-shadow:\s*var\(--shadow-panel/);
  assert.match(menubarPopover, /\.menubar-popover-host\s*{[\s\S]*overflow:\s*hidden;/);

  assert.match(devMenubarPopover, /\.popover-frame\s*{[\s\S]*width:\s*280px;/);
  assert.match(devMenubarPopover, /\.popover-frame\s*{[\s\S]*border-radius:\s*var\(--radius-card\);/);
  assert.match(
    devMenubarPopover,
    /\.popover-frame\s*{[\s\S]*background:\s*linear-gradient\(180deg,\s*var\(--surface-raised\),\s*var\(--surface\)\);/
  );
  assert.match(devMenubarPopover, /\.popover-frame\s*{[\s\S]*border:\s*1px solid var\(--hairline\);/);
  assert.match(devMenubarPopover, /\.popover-frame\s*{[\s\S]*box-shadow:\s*var\(--shadow-panel/);
  assert.match(devMenubarPopover, /\.popover-frame\s*{[\s\S]*overflow:\s*hidden;/);
});
