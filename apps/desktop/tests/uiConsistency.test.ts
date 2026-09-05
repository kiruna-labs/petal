import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { accessSync, constants, readFileSync, readdirSync } from 'node:fs';
import { mkdtemp, rm } from 'node:fs/promises';
import { homedir, tmpdir } from 'node:os';
import { basename, join, resolve, sep } from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { build } from 'vite';

// ---------------------------------------------------------------------------
// Static sweep over shipped Svelte CSS: every shipped surface must be driven
// by tokens.css, with only the comp-cited / transient literals below allowed.
//
// "Shipped" = src/lib/components/*.svelte + src/routes/**/*.svelte minus
// src/routes/dev/**. The hover-tab route is exempt from the sweep entirely:
// its var() fallbacks are read verbatim by tests/hoverTabOptionsMenu.test.ts
// (Step 7 of the sweep plan — never strip them).
// ---------------------------------------------------------------------------

const componentsDir = new URL('../src/lib/components/', import.meta.url);
const routesDir = new URL('../src/routes/', import.meta.url);
const tokensSource = readFileSync(new URL('../../../shared/ui/tokens.css', import.meta.url), 'utf8');
const regionWindowSource = readFileSync(
  new URL('../src/routes/region-window/+page.svelte', import.meta.url),
  'utf8'
);

/** Collect shipped .svelte files: components (flat) + routes (recursive, no dev). */
function shippedSvelteFiles(): string[] {
  const files: string[] = [];
  for (const entry of readdirSync(componentsDir)) {
    if (entry.endsWith('.svelte')) files.push(fileURLToPath(new URL(entry, componentsDir)));
  }
  const walk = (dir: URL): void => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      if (entry.name === 'dev') continue;
      if (entry.isDirectory()) walk(new URL(`${entry.name}/`, dir));
      else if (entry.name.endsWith('.svelte')) files.push(fileURLToPath(new URL(entry.name, dir)));
    }
  };
  walk(routesDir);
  return files.sort();
}

/** The <style> blocks of one Svelte file, with CSS comments removed. */
function styleTextOf(source: string): string {
  const blocks: string[] = [];
  const stylePattern = /<style[^>]*>([\s\S]*?)<\/style>/g;
  for (const match of source.matchAll(stylePattern)) blocks.push(match[1]);
  return blocks.join('\n').replace(/\/\*[\s\S]*?\*\//g, '');
}

// ---- Allowlists (each entry mirrors Step 0 / the review decisions) ---------

/**
 * Hex-color allowlist, keyed by file, matched against the *containing line*
 * of each found hex so the comment text above a literal can't shadow it.
 * Only Apple's system traffic-light colors and the compositor's transient
 * overlay-chrome accents stay literal.
 */
const HEX_ALLOWLIST: Array<{ file: string; pattern: RegExp; comment: string }> = [
  {
    file: 'RemoteWindowHeader.svelte',
    pattern: /background: #(?:ff5f57|febc2e|28c840);/,
    comment: 'Apple system traffic-light colors (comp-standard)'
  },
  {
    file: 'MainMenu.svelte',
    pattern: /background: #(?:ff5f57|febc2e);/,
    comment: 'Apple system traffic-light colors — main-window hide/minimize dots'
  },
  {
    file: join('compositor', 'control', '+page.svelte'),
    pattern: /color: #(?:82aaff|ff6b7d);/,
    comment: 'compositor overlay-chrome debug accents (transient, over video)'
  },
  {
    file: 'Pill.svelte',
    pattern: /background: linear-gradient\(180deg, #161618, #060607\);/,
    comment: 'pill attach sheet — pinned verbatim by hoverTabOptionsMenu (fixed-tab contrast)'},{
  }
];

/**
 * White-alpha allowlist (declaration-line match). Values outside the token
 * ramp {0.04,0.06,0.07,0.08,0.10,0.45,0.62,0.75,0.88} stay literal only when
 * the line is comp-cited (speaking rings, pill attach insets), transient
 * overlay chrome (compositor), or an emphasis/state value with no token
 * (focus borders/outlines, near-white button, status dot, action pill).
 */
const WHITE_ALPHA_ALLOWED: Record<string, true> = {
  '0.04': true,
  '0.06': true,
  '0.07': true,
  '0.08': true,
  '0.10': true,
  '0.45': true,
  '0.62': true,
  '0.75': true,
  '0.88': true
};
const WHITE_ALPHA_ALLOWLIST: Array<{ pattern: RegExp; comment: string }> = [
  {
    pattern: /rgba\(255, 255, 255, 0\.(?:55|22)\)/,
    comment: 'speaking rings (Avatar, MeetingChrome, ParticipantTile .spk)'
  },
  {
    pattern: /rgba\(255, 255, 255, 0\.(?:8|34)\)/,
    comment: 'speaking-breathe keyframes (ParticipantTile)'
  },
  {
    pattern: /rgba\(255, 255, 255, 0\.(?:2|09|07)\)/,
    comment: 'pill attach inset stack (Pill.svelte + hover-tab)'
  },
  {
    pattern: /rgba\(255, 255, 255, 0\.(?:52|55|78)\)/,
    comment: 'compositor resize-handle fills'
  },
  {
    pattern: /rgba\(255, 255, 255, 0\.(?:14|22)\)/,
    comment: 'compositor overlay-chrome borders (debug close, control hint)'
  },
  {
    pattern: /border-color: rgba\(255, 255, 255, 0\.28\);/,
    comment: 'MainMenu join-input focus border (no hairline token reaches it)'
  },
  {
    pattern: /border-color: rgba\(255, 255, 255, 0\.18\);/,
    comment: 'DeviceSelect open-state border emphasis'
  },
  {
    pattern: /border-color: rgba\(255, 255, 255, 0\.25\);/,
    comment: 'IdentitySetup name-field focus border'
  },
  {
    pattern: /outline: 1px solid rgba\(255, 255, 255, 0\.(?:42|18)\);/,
    comment: 'Gallery focus/persistent outlines (no hairline token reaches 0.42)'
  },
  {
    pattern: /background: rgba\(255, 255, 255, 0\.94\);/,
    comment: 'MainMenu create-btn--light near-white surface (no white-surface token)'
  },
  {
    pattern: /background: rgba\(255, 255, 255, 0\.3\);/,
    comment: 'OfflineState status dot (no fill token reaches 0.3)'
  },
  {
    pattern: /background: rgba\(255, 255, 255, 0\.2\);/,
    comment: 'Toast action-pill emphasis (no fill token reaches 0.2)'
  }
];

/**
 * Box-shadow allowlist (declaration match). Everything else must reference
 * var(--shadow-*) or carry no literal color at all (rings of token colors,
 * `none`, transparent placeholders).
 */
const BOX_SHADOW_ALLOWLIST: Array<{ pattern: RegExp; comment: string }> = [
  {
    pattern: /rgba\(255, 255, 255, 0\.(?:55|22|8|34)\)/,
    comment: 'speaking rings'
  },
  {
    pattern: /rgba\(255, 255, 255, 0\.(?:2|09|07)\)/,
    comment: 'pill attach insets'
  },
  {
    pattern: /0 12px 28px rgba\(0, 0, 0, 0\.42\)/,
    comment: 'compositor hint-panel shadow'
  },
  {
    pattern: /0 0 10px 2px rgba\(130, 170, 255, 0\.6\)/,
    comment: 'compositor local-echo ripple glow'
  },
  {
    pattern: /0 8px 24px rgba\(0, 0, 0, 0\.14\)/,
    comment: 'PermissionRow up-next shadow'
  },
  {
    pattern: /0 14px 30px -24px rgba\(0, 0, 0, 0\.82\)/,
    comment: 'NetworkCockpit gauge-card shadow'
  },
  {
    pattern: /0 14px 34px rgba\(0, 0, 0, 0\.16\)/,
    comment: 'WindowPicker window-card shadow'
  },
  {
    pattern: /0 18px 44px rgba\(0, 0, 0, 0\.2\)/,
    comment: 'WindowPicker window-card hover shadow'
  },
  {
    pattern: /0 12px 28px -22px rgba\(0, 0, 0, 0\.9\)/,
    comment: 'WindowPicker preview-thumbnail shadow'
  },
  {
    pattern: /inset 0 0 0 0\.5px rgba\(0, 0, 0, 0\.25\)/,
    comment: 'RemoteWindowHeader traffic-dot pressed inset'
  },
  {
    pattern: /inset 0 1px 2px rgba\(0, 0, 0, 0\.35\)/,
    comment: 'RemoteWindowHeader header-btn pressed inset'
  },
  {
    pattern: /0 1px 2px rgba\(0, 0, 0, 0\.35\)/,
    comment: 'RemoteWindowHeader mode-switcher segment shadow'
  },
  {
    pattern: /0 2px 8px rgba\(0, 0, 0, 0\.25\)/,
    comment: 'Pointer.svelte label shadow (transient telepointer overlay)'
  },
  {
    pattern: /inset 0 0 0 1px rgba\(0, 0, 0, 0\.16\)/,
    comment: 'menubar-popover remote-icon checkmark inset'
  }
];

// ---- Token pins (Step 8.1) ------------------------------------------------

test('tokens.css pins every sweep token with its approved value', () => {
  const pins: Array<[string, RegExp]> = [
    // Shape (Step 1 additions + the two value changes)
    ['--radius-shell', /--radius-shell:\s*24px;/],
    ['--radius-menu', /--radius-menu:\s*20px;/],
    ['--radius-popover', /--radius-popover:\s*14px;/],
    ['--radius-control', /--radius-control:\s*12px;/],
    ['--radius-input', /--radius-input:\s*10px;/],
    ['--radius-badge', /--radius-badge:\s*5px;/],
    ['--radius-tile', /--radius-tile:\s*16px;/],
    // Motion roles
    ['--motion-feedback', /--motion-feedback:\s*120ms;/],
    ['--motion-exit', /--motion-exit:\s*120ms;/],
    ['--motion-enter', /--motion-enter:\s*180ms;/],
    ['--motion-layout', /--motion-layout:\s*220ms;/],
    ['--motion-distance', /--motion-distance:\s*4px;/],
    ['--focus-ring', /--focus-ring:\s*var\(--id-blue\);/],
    ['--focus-ring-width', /--focus-ring-width:\s*2px;/],
    ['--focus-ring-offset', /--focus-ring-offset:\s*2px;/],
    ['--disabled-opacity', /--disabled-opacity:\s*0\.38;/],
    ['--hairline', /--hairline:\s*rgba\(255,\s*255,\s*255,\s*0\.07\);/],
    // White-alpha ladders
    ['--fill-weak', /--fill-weak:\s*rgba\(255,\s*255,\s*255,\s*0\.04\);/],
    ['--fill-base', /--fill-base:\s*rgba\(255,\s*255,\s*255,\s*0\.06\);/],
    ['--fill-strong', /--fill-strong:\s*rgba\(255,\s*255,\s*255,\s*0\.08\);/],
    ['--fill-bright', /--fill-bright:\s*rgba\(255,\s*255,\s*255,\s*0\.10\);/],
    ['--text-dim', /--text-dim:\s*rgba\(255,\s*255,\s*255,\s*0\.62\);/],
    ['--text-soft', /--text-soft:\s*rgba\(255,\s*255,\s*255,\s*0\.75\);/],
    ['--text-strong', /--text-strong:\s*rgba\(255,\s*255,\s*255,\s*0\.88\);/],
    // Surfaces
    ['--popover-bg', /--popover-bg:\s*linear-gradient\(180deg,\s*var\(--surface-raised\),\s*var\(--surface\)\);/],
    ['--graphite-gradient', /--graphite-gradient:\s*linear-gradient\(160deg,\s*#202124,\s*#17181b\);/],
    ['--hero-gradient', /--hero-gradient:\s*linear-gradient\(165deg,\s*#16181b,\s*#0b0c0e 80%\);/],
    ['--hero-gradient-live', /--hero-gradient-live:\s*linear-gradient\(165deg,\s*#123021,\s*#0b140e 78%\);/],
    [
      '--hero-bloom-live',
      /--hero-bloom-live:\s*radial-gradient\(58% 80% at 82% 24%,\s*rgba\(52,\s*199,\s*89,\s*0\.30\),\s*transparent 68%\);/],

    ['--mix-base', /--mix-base:\s*#0c0c0e;/],
    ['--glass-chip', /--glass-chip:\s*rgba\(8,\s*10,\s*12,\s*0\.68\);/],
    ['--glass-name', /--glass-name:\s*rgba\(8,\s*10,\s*12,\s*0\.55\);/],
    ['--glass-panel', /--glass-panel:\s*rgba\(20,\s*22,\s*24,\s*0\.92\);/],
    ['--glass-panel-strong', /--glass-panel-strong:\s*rgba\(20,\s*22,\s*24,\s*0\.94\);/],
    ['--glass-filmstrip', /--glass-filmstrip:\s*rgba\(15,\s*16,\s*19,\s*0\.72\);/],
    // Live CTA
    ['--cta-live-bg', /--cta-live-bg:\s*#34c759;/],
    ['--cta-live-ink', /--cta-live-ink:\s*#06280f;/],
    ['--cta-live-shadow', /--cta-live-shadow:\s*0 6px 22px -6px rgba\(52,\s*199,\s*89,\s*0\.6\);/],
    ['--live-soft', /--live-soft:\s*#5fe084;/],
    ['--live-face-bg', /--live-face-bg:\s*#274031;/],
    ['--live-face-ring', /--live-face-ring:\s*#0e1a12;/],
    ['--live-face-ink', /--live-face-ink:\s*#9fe6b4;/],
    // Shadows
    ['--shadow-menu', /--shadow-menu:\s*0 40px 90px -30px rgba\(0,\s*0,\s*0,\s*0\.6\);/],
    ['--shadow-tooltip', /--shadow-tooltip:\s*0 10px 28px rgba\(0,\s*0,\s*0,\s*0\.28\);/]
  ];
  for (const [name, pattern] of pins) {
    assert.match(tokensSource, pattern, `missing or drifted token: ${name}`);
  }
});

// ---- Sweep contract (Step 8.2) ---------------------------------------------

test('shipped Svelte CSS is fully token-driven (no literal colors, radii, shadows, fonts)', () => {
  const failures: string[] = [];
  const fileLabel = (file: string): string => file.split(`${sep}src${sep}`).pop() ?? file;

  for (const file of shippedSvelteFiles()) {
    // hover-tab fallbacks are pinned by tests/hoverTabOptionsMenu.test.ts.
    if (fileLabel(file).endsWith(join('hover-tab', '+page.svelte'))) continue;

    const label = fileLabel(file);
    const source = readFileSync(file, 'utf8');
    const style = styleTextOf(source);
    if (!style) continue;

    // 1. No hex colors (allowlist: RemoteWindowHeader traffic dots,
    //    compositor overlay-chrome accents).
    const hexPattern = /#[0-9a-fA-F]{3,8}\b/g;
    for (const match of style.matchAll(hexPattern)) {
      const lineStart = style.lastIndexOf('\n', match.index) + 1;
      const lineEnd = style.indexOf('\n', match.index);
      const line = style.slice(lineStart, lineEnd === -1 ? undefined : lineEnd);
      const allowlisted = HEX_ALLOWLIST.some(({ file: allowFile, pattern }) => {
        if (!label.endsWith(allowFile)) return false;
        pattern.lastIndex = 0;
        return pattern.test(line);
      });
      if (!allowlisted) failures.push(`${label}: literal hex ${match[0]} at: ${line.trim()}`);
    }

    // 2. No off-scale literal border-radius. Multi-value compositions like
    //    NetworkCockpit's gauge dome and ParticipantTile's silhouette are
    //    comp-cited shapes, not tokenizable surfaces, so they remain named
    //    exceptions rather than silently becoming a second radius system.
    const radiusPattern = /\bborder-radius:\s*\d+px\s*;/g;
    for (const match of style.matchAll(radiusPattern)) {
      failures.push(`${label}: literal border-radius "${match[0].trim()}"`);
    }
    const multiRadiusPattern = /\bborder-radius:\s*([^;]*\d+px[^;]*);/g;
    for (const match of style.matchAll(multiRadiusPattern)) {
      const declaration = match[0].replace(/\s+/g, ' ').trim();
      const allowed =
        (label.endsWith('Avatar.svelte') && declaration === 'border-radius: 999px 999px 42% 42%;') ||
        (label.endsWith('ParticipantTile.svelte') && declaration === 'border-radius: 80px 80px 0 0;') ||
        (label.endsWith('NetworkCockpit.svelte') && declaration === 'border-radius: 13px 13px 999px 999px;');
      if (!allowed && !declaration.includes('var(--')) {
        failures.push(`${label}: off-scale border-radius "${declaration}"`);
      }
    }

    // 3. No raw rgb() colors; semantic colors belong in tokens or explicit
    //    allowlisted overlay exceptions just like hex/rgba values.
    const rgbPattern = /\brgb\([^)]*\)/g;
    for (const match of style.matchAll(rgbPattern)) {
      failures.push(`${label}: literal rgb color ${match[0]}`);
    }

    // 4. Every font-family declaration references a --font-* token.
    const fontPattern = /\bfont-family:\s*[^;]*;/g;
    for (const match of style.matchAll(fontPattern)) {
      if (!match[0].includes('var(--font-')) {
        failures.push(`${label}: font-family without a --font-* token: ${match[0].trim()}`);
      }
    }

    // 5. Every box-shadow references var(--shadow-*) or carries no literal
    //    color (rings of token colors, none, transparent) or is allowlisted.
    const shadowPattern = /\bbox-shadow:\s*[^;]*;/g;
    for (const match of style.matchAll(shadowPattern)) {
      const declaration = match[0];
      if (declaration.includes('var(--shadow-')) continue;
      const hasLiteralColor = /(?:#|rgba\(|rgb\(|hsl\()/.test(declaration);
      if (!hasLiteralColor) continue;
      const allowlisted = BOX_SHADOW_ALLOWLIST.some(({ pattern }) => {
        pattern.lastIndex = 0;
        return pattern.test(declaration);
      });
      if (!allowlisted) {
        failures.push(`${label}: box-shadow without a shadow token: ${declaration.replace(/\s+/g, ' ').trim()}`);
      }
    }

    // 6. Every white-alpha literal sits in the token ramp or on an allowlist.
    const alphaPattern = /rgba\(\s*255\s*,\s*255\s*,\s*255\s*,\s*([0-9.]+)\s*\)/g;
    for (const match of style.matchAll(alphaPattern)) {
      const alpha = match[1];
      if (WHITE_ALPHA_ALLOWED[alpha]) continue;
      const lineStart = style.lastIndexOf('\n', match.index) + 1;
      const lineEnd = style.indexOf('\n', match.index);
      const line = style.slice(lineStart, lineEnd === -1 ? undefined : lineEnd);
      const allowlisted = WHITE_ALPHA_ALLOWLIST.some(({ pattern }) => {
        pattern.lastIndex = 0;
        return pattern.test(line);
      });
      if (!allowlisted) failures.push(`${label}: off-ramp white alpha ${alpha} at: ${line.trim()}`);
    }

    // 7. No var(--X, <hex|rgba|rgb>) fallbacks (dead code — tokens load
    //    unconditionally via app.css).
    const fallbackPattern = /var\(--[\w-]+\s*,\s*[^)]*(?:#[0-9a-fA-F]{3,8}|rgba\(|rgb\()/g;
    for (const match of style.matchAll(fallbackPattern)) {
      failures.push(`${label}: var() fallback with a literal: ${match[0].trim()}`);
    }
  }

  assert.deepEqual(
    failures,
    [],
    `Sweep contract violations (${failures.length}):\n${failures.join('\n')}`
  );
});

// ---- Focus-within regression (sticky hover/tooltip fix) -------------------

// Mouse interactions leave focus on the interacted element (right-button
// mousedown focuses buttons and role="button" divs; left-drag-release never
// moves focus), so any :focus-within reveal rule would stay applied forever
// after a mouse gesture. Reveal rules must key on :has(:focus-visible) —
// true only for genuine keyboard focus (Tab) — and clear via the paired
// :hover rules on mousemove.
test('shipped Svelte CSS never uses :focus-within (mouse focus must not stick reveal styles)', () => {
  for (const file of shippedSvelteFiles()) {
    const label = file.split(`${sep}src${sep}`).pop() ?? file;
    assert.doesNotMatch(
      styleTextOf(readFileSync(file, 'utf8')),
      /:focus-within/,
      `${label} pins :focus-within — a mouse gesture (right-click, left-drag release) focuses the element and sticks its reveal styles; use :has(:focus-visible)`
    );
  }
});

test('user-facing shipped styles do not introduce ellipsis truncation', () => {
  const diagnosticExceptions = ['NetworkCockpit.svelte', 'TestCockpitResults.svelte'];
  const failures: string[] = [];
  for (const file of shippedSvelteFiles()) {
    const label = file.split(`${sep}src${sep}`).pop() ?? file;
    if (diagnosticExceptions.some((exception) => label.endsWith(exception))) continue;
    if (/text-overflow:\s*ellipsis/.test(styleTextOf(readFileSync(file, 'utf8')))) {
      failures.push(label);
    }
  }
  assert.deepEqual(failures, [], `user-facing ellipsis remains in: ${failures.join(', ')}`);
});

test('Petal View region selector uses the shared chrome language without losing boundary contrast', () => {
  assert.match(regionWindowSource, /CloseButton ariaLabel="Close region selector"/);
  assert.doesNotMatch(regionWindowSource, /share-region-outline|danger-solid/);
  assert.doesNotMatch(regionWindowSource, /TOP_ZONE\s*=\s*24|TOP_ZONE_PX/);
  assert.match(regionWindowSource, /const FRAME_INSET = 6;/);
  assert.match(regionWindowSource, /const TITLE_BAR_MIN_HEIGHT = 56;/);
  assert.match(regionWindowSource, /bind:this=\{titleBar\}/);
  assert.match(regionWindowSource, /new ResizeObserver\(syncTitleBoundary\)/);
  assert.match(regionWindowSource, /titleBar\?\.getBoundingClientRect\(\)/);
  assert.match(regionWindowSource, /y <= titleBottomPx/);
  assert.match(
    regionWindowSource,
    /\.hollow-frame\s*\{[\s\S]*?border:\s*3px solid var\(--bg-base\);[\s\S]*?border-radius:\s*var\(--radius-card\);[\s\S]*?overflow:\s*hidden;[\s\S]*?padding:\s*3px;/
  );
  assert.match(
    regionWindowSource,
    /\.hollow-frame::before\s*\{[\s\S]*?inset:\s*0;[\s\S]*?border:\s*3px solid var\(--text-primary\);[\s\S]*?border-radius:\s*calc\(var\(--radius-card\) - 3px\);[\s\S]*?pointer-events:\s*none;/
  );
  assert.doesNotMatch(regionWindowSource, /\.hollow-frame::after/);
  assert.doesNotMatch(regionWindowSource, /\.hollow-frame::before\s*\{[^}]*box-shadow:/);
  assert.match(
    regionWindowSource,
    /min-height:\s*56px;[\s\S]*?padding:\s*8px 8px 8px 10px;[\s\S]*?box-shadow:\s*inset 0 -1px 0 var\(--hairline-strong\);/
  );
  assert.match(regionWindowSource, /border-radius:\s*var\(--radius-input\) var\(--radius-input\) 0 0;/);
  assert.match(regionWindowSource, /font:\s*600 var\(--text-micro\) \/ 16px var\(--font-ui\);/);
  assert.match(regionWindowSource, /line-height:\s*16px;/);
  assert.match(
    regionWindowSource,
    /gap:\s*8px;[\s\S]*?margin:\s*8px 10px;[\s\S]*?padding:\s*8px;[\s\S]*?font:\s*700 var\(--text-micro\) \/ 16px var\(--font-ui\);/
  );
  assert.match(regionWindowSource, /\.warning-icon\s*\{[\s\S]*?width:\s*16px;[\s\S]*?height:\s*16px;/);
  assert.match(regionWindowSource, /in:fly=\{\{ y: 4, duration: enterDuration\(\) \}\}/);
  assert.match(regionWindowSource, /out:fade=\{\{ duration: exitDuration\(\) \}\}/);
  assert.match(regionWindowSource, /overflow-wrap: anywhere;/);
  assert.match(regionWindowSource, /\.title-bar :global\(\.close-button:hover/);
  assert.match(regionWindowSource, /\.title-bar :global\(\.close-button\)[\s\S]*?z-index:\s*3;/);
});

// ---- Rendered dual-UA case (Step 8.3) --------------------------------------

// Both platform cases force a realistic UA via Emulation.setUserAgentOverride:
// the default headless UA follows the HOST OS, so it cannot stand in for
// either platform deterministically.
const WINDOWS_WEBVIEW2_UA =
  'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 ' +
  '(KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0';
const MACOS_WKWEVIEW_UA =
  'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 ' +
  '(KHTML, like Gecko) Version/17.4 Safari/605.1.15';
const VIEWPORT = { width: 400, height: 800 };

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);

/** CDP response envelope — arrives untyped over the debug pipe, shaped minimally. */
interface CdpResponse {
  exceptionDetails?: {
    exception?: { description?: string };
    text?: string;
  };
  result?: { value?: unknown };
}

interface RenderedTestBrowser {
  call: (method: string, params?: Record<string, unknown>, sessionId?: string) => Promise<unknown>;
  evaluate: (sessionId: string, expression: string) => Promise<unknown>;
  stderr: () => string;
  close: () => Promise<void>;
}

function cachedChromiumCandidates(): string[] {
  const cacheRoots = [
    join(homedir(), 'Library', 'Caches', 'ms-playwright'),
    join(homedir(), '.cache', 'ms-playwright'),
    join(homedir(), 'AppData', 'Local', 'ms-playwright')
  ];
  const platformDirs =
    process.platform === 'darwin'
      ? [process.arch === 'arm64' ? 'chrome-headless-shell-mac-arm64' : 'chrome-headless-shell-mac-x64']
      : process.platform === 'linux' && process.arch === 'x64'
        ? ['chrome-headless-shell-linux64']
        : process.platform === 'win32' && process.arch === 'x64'
          ? ['chrome-headless-shell-win64']
          : [];
  const executableName = process.platform === 'win32' ? 'chrome-headless-shell.exe' : 'chrome-headless-shell';
  const candidates: string[] = [];
  for (const root of cacheRoots) {
    let entries: string[] = [];
    try {
      entries = readdirSync(root).filter((entry) => entry.startsWith('chromium_headless_shell-'));
    } catch {
      continue;
    }
    for (const entry of entries.sort().reverse()) {
      for (const platformDir of platformDirs) {
        candidates.push(join(root, entry, platformDir, executableName));
      }
    }
  }
  return candidates;
}

function renderedTestBrowser(): string {
  const candidates = [
    process.env.PETAL_CHROME_BIN,
    ...cachedChromiumCandidates(),
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/usr/bin/google-chrome',
    '/usr/bin/chromium',
    '/usr/bin/chromium-browser'
  ].filter((candidate): candidate is string => Boolean(candidate));
  const browser = candidates.find((candidate) => {
    try {
      accessSync(candidate, constants.X_OK);
      return true;
    } catch {
      return false;
    }
  });
  assert.ok(
    browser,
    `rendered main-platform test requires Chromium; checked: ${candidates.join(', ')}`
  );
  return browser;
}

function withTimeout<T>(promise: Promise<T>, timeoutMs: number, label: string): Promise<T> {
  const { promise: settled, resolve, reject } = Promise.withResolvers<T>();
  const timer = setTimeout(() => reject(new Error(`${label} timed out after ${timeoutMs}ms`)), timeoutMs);
  promise.then(
    (value) => {
      clearTimeout(timer);
      resolve(value);
    },
    (error) => {
      clearTimeout(timer);
      reject(error);
    }
  );
  return settled;
}

async function removeTempPath(path: string): Promise<void> {
  for (let attempt = 0; attempt < 12; attempt += 1) {
    try {
      await rm(path, { recursive: true, force: true });
      return;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== 'EBUSY' || attempt === 11) throw error;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }
}

async function launchRenderedTestBrowser(profileDir: string): Promise<RenderedTestBrowser> {
  const browserPath = renderedTestBrowser();
  const browserArgs = [
    '--headless',
    // Multi-process headless (the Settings fixture needs it on Windows; keep
    // the same flags so both rendered fixtures share one launch path).
    '--no-zygote',
    '--no-sandbox',
    '--disable-gpu',
    '--disable-software-rasterizer',
    '--disable-background-networking',
    '--disable-background-timer-throttling',
    '--disable-backgrounding-occluded-windows',
    '--disable-renderer-backgrounding',
    '--allow-file-access-from-files',
    '--force-device-scale-factor=1',
    '--no-first-run',
    '--no-default-browser-check',
    `--user-data-dir=${profileDir}`,
    '--remote-debugging-pipe',
    '--no-startup-window'
  ];
  const command = process.platform === 'darwin' && process.arch === 'arm64' && basename(browserPath) === 'Google Chrome'
    ? '/usr/bin/arch'
    : browserPath;
  const args = command === '/usr/bin/arch' ? ['-arm64', browserPath, ...browserArgs] : browserArgs;
  const child = spawn(command, args, { stdio: ['ignore', 'ignore', 'pipe', 'pipe', 'pipe'] });
  const { promise: browserExited, resolve: resolveBrowserExit } = Promise.withResolvers<void>();
  child.once('exit', () => resolveBrowserExit());
  let stderr = '';
  child.stderr.on('data', (chunk) => {
    stderr = `${stderr}${chunk}`.slice(-8000);
  });

  let nextId = 1;
  let buffer = Buffer.alloc(0);
  const pending = new Map<number, {
    resolve: (value: unknown) => void;
    reject: (error: Error) => void;
    timer: ReturnType<typeof setTimeout>;
  }>();

  function rejectPending(error: Error) {
    for (const waiter of pending.values()) {
      clearTimeout(waiter.timer);
      waiter.reject(error);
    }
    pending.clear();
  }

  child.once('error', (error) => rejectPending(error));
  child.once('exit', (code, signal) => {
    if (pending.size > 0) {
      rejectPending(
        new Error(
          `rendered-test browser exited before replying (code=${code}, signal=${signal})\n${stderr}`
        )
      );
    }
  });

  const protocolInput = child.stdio[3];
  const protocolOutput = child.stdio[4];
  assert.ok(protocolInput && protocolOutput, 'Chromium did not expose its remote-debugging pipes');
  protocolOutput.on('data', (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    for (;;) {
      const delimiter = buffer.indexOf(0);
      if (delimiter < 0) break;
      const rawMessage = buffer.subarray(0, delimiter).toString();
      buffer = buffer.subarray(delimiter + 1);
      if (!rawMessage) continue;
      const message = JSON.parse(rawMessage) as {
        id?: number;
        error?: { message?: string };
        result?: unknown;
      };
      if (!message.id) continue;
      const waiter = pending.get(message.id);
      if (!waiter) continue;
      pending.delete(message.id);
      clearTimeout(waiter.timer);
      if (message.error) waiter.reject(new Error(message.error.message));
      else waiter.resolve(message.result);
    }
  });

  function call(method: string, params: Record<string, unknown> = {}, sessionId?: string): Promise<unknown> {
    const id = nextId++;
    const message: Record<string, unknown> = { id, method, params };
    if (sessionId) message.sessionId = sessionId;
    const { promise, resolve, reject } = Promise.withResolvers<unknown>();
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`${method} timed out\n${stderr}`));
    }, 10_000);
    pending.set(id, { resolve, reject, timer });
    protocolInput.write(`${JSON.stringify(message)}\0`);
    return promise;
  }

  async function evaluate(sessionId: string, expression: string): Promise<unknown> {
    const result = (await call(
      'Runtime.evaluate',
      { expression, awaitPromise: true, returnByValue: true },
      sessionId
    )) as CdpResponse;
    if (result.exceptionDetails) {
      throw new Error(
        result.exceptionDetails.exception?.description ??
          result.exceptionDetails.text ??
          'browser evaluation failed'
      );
    }
    return result.result?.value;
  }

  return {
    call,
    evaluate,
    stderr: () => stderr,
    async close() {
      if (child.exitCode !== null || child.signalCode !== null) return;
      child.kill('SIGTERM');
      try {
        await withTimeout(browserExited, 3000, 'Chromium shutdown');
      } catch {
        child.kill('SIGKILL');
        await withTimeout(browserExited, 3000, 'forced Chromium shutdown');
      }
    }
  };
}

test('main menu preserves shared colors and in-app shape tokens across platforms', async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-main-platform-build-'));
  const profileDir = await mkdtemp(join(tmpdir(), 'petal-main-platform-chrome-'));
  let browser: RenderedTestBrowser | undefined;

  try {
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      esbuild: {
        tsconfigRaw: JSON.stringify({
          compilerOptions: { target: 'ES2022', useDefineForClassFields: true }
        })
      },
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: {
        alias: {
          $lib: resolve(fileURLToPath(new URL('./src/lib', desktopRoot))),
          // Standalone Vite build skips SvelteKit config, so provide the
          // browser-only virtual module used by session.svelte.
          '$app/environment': fileURLToPath(new URL('./sveltekit-environment.ts', fixtureRoot)),
          // ...and the shared-package alias (SvelteKit's kit.alias injects it
          // in the real build; this bare Vite instance needs it manually).
          '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot)))
        }
      },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: {
          input: fileURLToPath(new URL('./main-platform.html', fixtureRoot))
        }
      }
    });

    browser = await launchRenderedTestBrowser(profileDir);
    const fixtureUrl = pathToFileURL(join(buildDir, 'main-platform.html')).href;

    const scenarios = [
      { name: 'windows', userAgent: WINDOWS_WEBVIEW2_UA },
      { name: 'macos', userAgent: MACOS_WKWEVIEW_UA }
    ];

    for (const scenario of scenarios) {
      const { targetId } = (await browser.call('Target.createTarget', {
        url: 'about:blank',
        width: VIEWPORT.width,
        height: VIEWPORT.height
      })) as { targetId: string };
      const { sessionId } = (await browser.call('Target.attachToTarget', {
        targetId,
        flatten: true
      })) as { sessionId: string };
      await browser.call(
        'Emulation.setDeviceMetricsOverride',
        {
          width: VIEWPORT.width,
          height: VIEWPORT.height,
          deviceScaleFactor: 1,
          mobile: false,
          screenWidth: VIEWPORT.width,
          screenHeight: VIEWPORT.height,
          dontSetVisibleSize: false
        },
        sessionId
      );
      await browser.call(
        'Emulation.setUserAgentOverride',
        { userAgent: scenario.userAgent },
        sessionId
      );
      await browser.call('Page.navigate', { url: fixtureUrl }, sessionId);

      // Real browser render — the page paints asynchronously and only signals
      // readiness through the DOM, so poll with a short deadline.
      const renderDeadline = Date.now() + 10_000;
      let rendered = false;
      while (Date.now() < renderDeadline) {
        const state = (await browser.evaluate(
          sessionId,
          `({
            rendered: document.body?.dataset.mainRendered ?? null,
            error: document.body?.dataset.mainRenderedError ?? null
          })`
        )) as { rendered?: string | null; error?: string | null } | null;
        if (state?.error) {
          throw new Error(`rendered main-platform fixture failed: ${decodeURIComponent(state.error)}`);
        }
        if (state?.rendered) {
          rendered = true;
          break;
        }
        const remainingMs = renderDeadline - Date.now();
        if (remainingMs > 0) {
          const { promise: frame, resolve: resolveFrame } = Promise.withResolvers<void>();
          setTimeout(resolveFrame, Math.min(50, remainingMs));
          await frame;
        }
      }
      if (!rendered) {
        throw new Error(
          `${scenario.name} main-platform render timed out after 10000ms\n${browser.stderr()}`
        );
      }

      // Shared color behavior remains identical across UAs, while the
      // platform profile intentionally changes semantic surface radii.
      const computed = (await browser.evaluate(
        sessionId,
        `(() => {
          const menu = document.querySelector('.main-menu');
          const create = document.querySelector('.create-btn');
          const joinInput = document.querySelector('.join-input');
          return {
            createBg: create ? getComputedStyle(create).backgroundColor : null,
            createRadius: create ? getComputedStyle(create).borderTopLeftRadius : null,
            joinRadius: joinInput ? getComputedStyle(joinInput).borderTopLeftRadius : null,
            menuRadius: menu ? getComputedStyle(menu).borderTopLeftRadius : null,
            colorScheme: getComputedStyle(document.documentElement).colorScheme
          };
        })()`
      )) as {
        createBg: string | null;
        createRadius: string | null;
        joinRadius: string | null;
        menuRadius: string | null;
        colorScheme: string;
      };

      const expectedShape = { menu: '20px', input: '10px', control: '12px' };
      assert.equal(computed.createBg, 'rgb(52, 199, 89)', `${scenario.name} green CTA background`);
      assert.equal(computed.menuRadius, expectedShape.menu, `${scenario.name} main-menu radius`);
      assert.equal(computed.joinRadius, expectedShape.input, `${scenario.name} join-input radius`);
      assert.equal(computed.createRadius, expectedShape.control, `${scenario.name} create-control radius`);
      assert.equal(computed.colorScheme, 'dark', `${scenario.name} color-scheme`);

      await browser.call('Target.closeTarget', { targetId });
    }
  } finally {
    try {
      await browser?.close();
    } finally {
      await Promise.all([
        removeTempPath(buildDir),
        removeTempPath(profileDir)
      ]);
    }
  }
});

test('region selector renders tokenized chrome and keeps long titles inside both platform viewports', { timeout: 30_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-region-window-build-'));
  const profileDir = await mkdtemp(join(tmpdir(), 'petal-region-window-chrome-'));
  let browser: RenderedTestBrowser | undefined;

  try {
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      esbuild: {
        tsconfigRaw: JSON.stringify({
          compilerOptions: { target: 'ES2022', useDefineForClassFields: true }
        })
      },
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: {
        alias: {
          $lib: resolve(fileURLToPath(new URL('./src/lib', desktopRoot))),
          '$app/environment': resolve(fileURLToPath(new URL('./sveltekit-environment.js', fixtureRoot))),
          '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot)))
        }
      },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: {
          input: fileURLToPath(new URL('./region-window.html', fixtureRoot))
        }
      }
    });

    browser = await launchRenderedTestBrowser(profileDir);
    const fixtureUrl = pathToFileURL(join(buildDir, 'region-window.html')).href;
    for (const scenario of [
      { name: 'windows', userAgent: WINDOWS_WEBVIEW2_UA },
      { name: 'macos', userAgent: MACOS_WKWEVIEW_UA }
    ]) {
      const { targetId } = (await browser.call('Target.createTarget', {
        url: 'about:blank',
        width: 640,
        height: 400
      })) as { targetId: string };
      const { sessionId } = (await browser.call('Target.attachToTarget', {
        targetId,
        flatten: true
      })) as { sessionId: string };
      await browser.call(
        'Emulation.setDeviceMetricsOverride',
        {
          width: 640,
          height: 400,
          deviceScaleFactor: 1,
          mobile: false,
          screenWidth: 640,
          screenHeight: 400,
          dontSetVisibleSize: false
        },
        sessionId
      );
      await browser.call('Emulation.setUserAgentOverride', { userAgent: scenario.userAgent }, sessionId);
      await browser.call('Page.navigate', { url: `${fixtureUrl}?ipcProbe=1&ipcDelayMs=5&shareDelayMs=120` }, sessionId);

      const deadline = Date.now() + 10_000;
      let rendered = false;
      let lastRenderState: { rendered?: string | null; error?: string | null } | null = null;
      while (Date.now() < deadline) {
        const state = (await browser.evaluate(
          sessionId,
          `({ rendered: document.body?.dataset.regionRendered ?? null, error: document.body?.dataset.regionRenderedError ?? null })`
        )) as { rendered?: string | null; error?: string | null } | null;
        lastRenderState = state;
        if (state?.error) {
          throw new Error(`${scenario.name} region fixture failed: ${decodeURIComponent(state.error)}`);
        }
        if (state?.rendered) {
          rendered = true;
          break;
        }
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
      assert.ok(
        rendered,
        `${scenario.name} region fixture did not render before the deadline: ${JSON.stringify(lastRenderState)}`
      );

      for (const background of ['light', 'dark', 'mixed']) {
        await browser.evaluate(sessionId, `document.body.dataset.regionBackground = '${background}'`);
        const measurement = (await browser.evaluate(
          sessionId,
          `(() => {
            const frame = document.querySelector('.hollow-frame');
            const title = document.querySelector('.title-label');
            const close = document.querySelector('.close-button');
            const share = document.querySelector('[data-region-share-control]');
            const titleBar = document.querySelector('.title-bar');
            const backdrop = document.querySelector('#desktop-backdrop');
            const style = frame ? getComputedStyle(frame) : null;
            const middle = frame ? getComputedStyle(frame, '::before') : null;
            const titleBarStyle = titleBar ? getComputedStyle(titleBar) : null;
            const frameRect = frame?.getBoundingClientRect();
            const titleBarRect = titleBar?.getBoundingClientRect();
            const labelRect = title?.getBoundingClientRect();
            const closeRect = close?.getBoundingClientRect();
            const centerY = (rect) => rect ? (rect.top + rect.bottom) / 2 : null;
            return {
              frameBorder: style?.borderTopWidth ?? null,
              frameBorderColor: style?.borderTopColor ?? null,
              frameRadius: style?.borderTopLeftRadius ?? null,
              framePaddingTop: style?.paddingTop ?? null,
              framePaddingLeft: style?.paddingLeft ?? null,
              frameOverflow: style?.overflow ?? null,
              frameShadow: style?.boxShadow ?? null,
              middleBorder: middle?.borderTopWidth ?? null,
              middleBorderColor: middle?.borderTopColor ?? null,
              middleTop: middle?.top ?? null,
              middleRadius: middle?.borderTopLeftRadius ?? null,
              middlePointerEvents: middle?.pointerEvents ?? null,
              shareBackground: share ? getComputedStyle(share).backgroundColor : null,
              shareColor: share ? getComputedStyle(share).color : null,
              shareLabel: share?.getAttribute('aria-label') ?? null,
              shareTitle: share?.getAttribute('title') ?? null,
              titleBackground: titleBarStyle?.backgroundColor ?? null,
              titleRadius: titleBarStyle?.borderTopLeftRadius ?? null,
              titleHeight: titleBarRect?.height ?? 0,
              titleInsetTop: titleBarRect && frameRect ? titleBarRect.top - frameRect.top : null,
              titleInsetLeft: titleBarRect && frameRect ? titleBarRect.left - frameRect.left : null,
              titleInsetRight: titleBarRect && frameRect ? frameRect.right - titleBarRect.right : null,
              titlePadding: titleBarStyle ? [titleBarStyle.paddingTop, titleBarStyle.paddingRight, titleBarStyle.paddingBottom, titleBarStyle.paddingLeft] : null,
              labelLeft: labelRect && frameRect ? labelRect.left - frameRect.left : null,
              labelLineHeight: title ? getComputedStyle(title).lineHeight : null,
              labelCenterY: centerY(labelRect),
              closeCenterY: centerY(closeRect),
              closeTopInset: closeRect && titleBarRect ? closeRect.top - titleBarRect.top : null,
              closeBottomInset: closeRect && titleBarRect ? titleBarRect.bottom - closeRect.bottom : null,
              closeRightInset: closeRect && frameRect ? frameRect.right - closeRect.right : null,
              titleWidth: title?.getBoundingClientRect().width ?? 0,
              closeWidth: closeRect?.width ?? 0,
              closeHeight: closeRect?.height ?? 0,
              closeRadius: close ? getComputedStyle(close).borderRadius : null,
              titleText: title?.textContent ?? null,
              backdropBackground: backdrop ? getComputedStyle(backdrop).backgroundImage : null,
              documentScrollWidth: document.documentElement.scrollWidth,
              documentClientWidth: document.documentElement.clientWidth,
              colorScheme: getComputedStyle(document.documentElement).colorScheme
            };
          })()`
        )) as {
          frameBorder: string | null;
          frameBorderColor: string | null;
          frameRadius: string | null;
          framePaddingTop: string | null;
          framePaddingLeft: string | null;
          frameOverflow: string | null;
          frameShadow: string | null;
          middleBorder: string | null;
          middleBorderColor: string | null;
          middleTop: string | null;
          middleRadius: string | null;
          middlePointerEvents: string | null;
          shareBackground: string | null;
          shareColor: string | null;
          shareLabel: string | null;
          shareTitle: string | null;
          titleBackground: string | null;
          titleRadius: string | null;
          titleHeight: number;
          titleInsetTop: number | null;
          titleInsetLeft: number | null;
          titleInsetRight: number | null;
          titlePadding: string[] | null;
          labelLeft: number | null;
          labelLineHeight: string | null;
          labelCenterY: number | null;
          closeCenterY: number | null;
          closeTopInset: number | null;
          closeBottomInset: number | null;
          closeRightInset: number | null;
          titleWidth: number;
          closeWidth: number;
          closeHeight: number;
          closeRadius: string | null;
          titleText: string | null;
          backdropBackground: string | null;
          documentScrollWidth: number;
          documentClientWidth: number;
          colorScheme: string;
        };

        assert.equal(measurement.frameBorder, '3px', `${scenario.name}/${background} outer frame width`);
        assert.equal(measurement.frameBorderColor, 'rgb(10, 10, 11)', `${scenario.name}/${background} outer frame color`);
        assert.equal(measurement.frameRadius, '16px', `${scenario.name}/${background} frame lost its outer Petal radius`);
        assert.equal(measurement.framePaddingTop, '3px', `${scenario.name}/${background} frame lost its reserved vertical inset`);
        assert.equal(measurement.framePaddingLeft, '3px', `${scenario.name}/${background} frame lost its reserved horizontal inset`);
        assert.equal(measurement.frameOverflow, 'hidden', `${scenario.name}/${background} frame no longer clips nested chrome`);
        assert.equal(measurement.frameShadow, 'none', `${scenario.name}/${background} frame regained an external shadow`);
        assert.equal(measurement.middleBorder, '3px', `${scenario.name}/${background} middle frame width`);
        assert.equal(measurement.middleBorderColor, 'rgb(245, 246, 247)', `${scenario.name}/${background} middle frame color`);
        assert.equal(measurement.middleTop, '0px', `${scenario.name}/${background} middle frame is not flush with the inner outer border`);
        assert.equal(measurement.middleRadius, '13px', `${scenario.name}/${background} middle frame radius`);
        assert.equal(measurement.middlePointerEvents, 'none', `${scenario.name}/${background} middle frame intercepted input`);
        assert.match(measurement.shareBackground ?? '', /rgba\(255, 255, 255, 0\.06\)/, `${scenario.name}/${background} idle Share control lost its graphite fill`);
        assert.equal(measurement.shareLabel, 'Share Petal View', `${scenario.name}/${background} idle Share control label drifted`);
        assert.equal(measurement.shareTitle, 'Share Petal View', `${scenario.name}/${background} idle Share control tooltip drifted`);
        assert.equal(measurement.titleRadius, '10px', `${scenario.name}/${background} title bar lost its 10px inner corner radius`);
        assert.equal(measurement.titleHeight, 56, `${scenario.name}/${background} one-line title bar is not 56px tall`);
        assert.equal(measurement.titleInsetTop, 6, `${scenario.name}/${background} title bar is not inside the complete frame`);
        assert.equal(measurement.titleInsetLeft, 6, `${scenario.name}/${background} title bar is not inside the complete frame`);
        assert.equal(measurement.titleInsetRight, 6, `${scenario.name}/${background} title bar does not end at the inner frame edge`);
        assert.deepEqual(measurement.titlePadding, ['8px', '8px', '8px', '10px'], `${scenario.name}/${background} title spacing drifted`);
        assert.equal(measurement.labelLeft, 16, `${scenario.name}/${background} title label lost its 16px optical inset`);
        assert.equal(measurement.labelLineHeight, '16px', `${scenario.name}/${background} title label baseline drifted`);
        assert.ok(
          Math.abs((measurement.labelCenterY ?? 0) - (measurement.closeCenterY ?? 0)) <= 0.5,
          `${scenario.name}/${background} title label and close icon are not vertically aligned`
        );
        assert.equal(measurement.closeTopInset, 8, `${scenario.name}/${background} close hover surface is not centered beside the 40px title actions`);
        assert.equal(measurement.closeBottomInset, 8, `${scenario.name}/${background} close hover surface is not centered beside the 40px title actions`);
        assert.equal(measurement.closeRightInset, 14, `${scenario.name}/${background} close surface lost its inner-ring clearance`);
        assert.equal(measurement.closeWidth, 40, `${scenario.name}/${background} close control lost its full hit target`);
        assert.equal(measurement.closeHeight, 40, `${scenario.name}/${background} close control lost its full hit target`);
        assert.equal(measurement.closeRadius, '8px', `${scenario.name}/${background} close control shape drifted`);
        assert.match(
          measurement.titleBackground,
          /0\.92/,
          `${scenario.name}/${background} title chrome lost its translucent graphite treatment`
        );
        assert.ok(measurement.backdropBackground, `${scenario.name}/${background} test backdrop disappeared`);
        assert.ok(measurement.titleWidth > 0, `${scenario.name}/${background} title disappeared`);
        assert.ok(measurement.titleText, `${scenario.name}/${background} title text disappeared`);
        assert.ok(
          measurement.documentScrollWidth <= measurement.documentClientWidth + 1,
          `${scenario.name}/${background} region chrome overflows its viewport`
        );
        assert.equal(measurement.colorScheme, 'dark', `${scenario.name}/${background} color scheme`);
      }

      await browser.evaluate(sessionId, `document.querySelector('[data-region-share-control]')?.click()`);
      await new Promise((resolve) => setTimeout(resolve, 20));
      const pendingControl = (await browser.evaluate(
        sessionId,
        `(() => {
          const share = document.querySelector('[data-region-share-control]');
          return {
            disabled: share instanceof HTMLButtonElement ? share.disabled : null,
            busy: share?.getAttribute('aria-busy') ?? null,
            invocations: window.__regionIpcProbe?.shareInvocations ?? 0
          };
        })()`
      )) as { disabled: boolean | null; busy: string | null; invocations: number };
      assert.equal(pendingControl.disabled, true, `${scenario.name} Share control was not disabled while native toggle was pending`);
      assert.equal(pendingControl.busy, 'true', `${scenario.name} Share control did not expose aria-busy while pending`);
      assert.equal(pendingControl.invocations, 1, `${scenario.name} pending Share did not invoke native toggle exactly once`);
      // Wait for the state to land AND for its CSS transitions to finish. A
      // fixed sleep here raced `.region-share-control`'s `background-color`/
      // `color` transition and sampled it mid-flight: the measured ink came
      // back `rgba(15, 23, 31, 0.99)`, exactly between the two accepted
      // endpoints, with the fractional alpha that gives an in-flight
      // transition away. Same defect class as #887.
      await browser.evaluate(
        sessionId,
        `(async () => {
          const deadline = Date.now() + 5000;
          const settled = () => {
            const root = document.querySelector('.window-container');
            const share = document.querySelector('[data-region-share-control]');
            return Boolean(root && share && /shared/.test(root.className)
              && share.getAttribute('aria-busy') === 'false');
          };
          while (Date.now() < deadline && !settled()) {
            await new Promise((r) => setTimeout(r, 16));
          }
          const targets = [
            document.querySelector('.window-container'),
            document.querySelector('.hollow-frame'),
            document.querySelector('[data-region-share-control]')
          ].filter(Boolean);
          while (Date.now() < deadline) {
            const running = targets.flatMap((el) => el.getAnimations());
            if (running.length === 0) break;
            await Promise.all(running.map((a) => a.finished.catch(() => {})));
          }
        })()`
      );
      const sharedControl = (await browser.evaluate(
        sessionId,
        `(() => {
          const root = document.querySelector('.window-container');
          const frame = document.querySelector('.hollow-frame');
          const share = document.querySelector('[data-region-share-control]');
          return {
            rootClass: root?.className ?? '',
            frameBorderColor: frame ? getComputedStyle(frame).borderTopColor : null,
            shareBackground: share ? getComputedStyle(share).backgroundColor : null,
            shareColor: share ? getComputedStyle(share).color : null,
            shareLabel: share?.getAttribute('aria-label') ?? null,
            shareTitle: share?.getAttribute('title') ?? null,
            shareBusy: share?.getAttribute('aria-busy') ?? null,
            shareInvocations: window.__regionIpcProbe?.shareInvocations ?? 0,
            commandHistory: window.__regionIpcProbe?.commandHistory ?? [],
            shareDisabled: share instanceof HTMLButtonElement ? share.disabled : null,
            shareOuterHtml: share?.outerHTML ?? null,
            routeLabel: root?.getAttribute('data-region-window-label'),
            routeSharePending: root?.getAttribute('data-region-share-pending'),
            nativeMetadataLabel: window.__TAURI_INTERNALS__?.metadata?.currentWindow?.label ?? null
          };
        })()`
      )) as {
        rootClass: string;
        frameBorderColor: string | null;
        shareBackground: string | null;
        shareColor: string | null;
        shareLabel: string | null;
        shareTitle: string | null;
        shareBusy: string | null;
        shareInvocations: number;
        commandHistory: string[];
      };
      assert.match(sharedControl.rootClass, /shared/, `${scenario.name} active Petal View state did not reach the frame: ${JSON.stringify(sharedControl)}`);
      assert.equal(sharedControl.frameBorderColor, 'rgb(143, 166, 184)', `${scenario.name} active frame lost identity color`);
      assert.match(sharedControl.shareBackground ?? '', /(?:143, 166, 184|144, 167, 184)/, `${scenario.name} active Share control lost identity fill`);
      assert.match(sharedControl.shareColor ?? '', /(?:7, 16, 24|24, 33, 40)/, `${scenario.name} active Share control lost identity ink`);
      assert.equal(sharedControl.shareLabel, 'Stop sharing Petal View', `${scenario.name} active Share label drifted`);
      assert.equal(sharedControl.shareTitle, 'Stop sharing Petal View', `${scenario.name} active Share tooltip drifted`);
      assert.equal(sharedControl.shareBusy, 'false', `${scenario.name} active Share remained busy`);
      assert.equal(sharedControl.shareInvocations, 1, `${scenario.name} direct Share did not use the native toggle exactly once`);
      await browser.evaluate(sessionId, `document.querySelector('[data-region-share-control]')?.click()`);
      await new Promise((resolve) => setTimeout(resolve, 80));

      await browser.call(
        'Emulation.setDeviceMetricsOverride',
        {
          width: 160,
          height: 120,
          deviceScaleFactor: 1,
          mobile: false,
          screenWidth: 160,
          screenHeight: 120,
          dontSetVisibleSize: false
        },
        sessionId
      );
      const compactTitle = 'A long Petal View title for compact-window wrapping';
      await browser.evaluate(
        sessionId,
        `document.querySelector('.title-label').textContent = ${JSON.stringify(compactTitle)}`
      );
      await browser.evaluate(
        sessionId,
        'new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)))'
      );
      const compact = (await browser.evaluate(
        sessionId,
        `(() => {
          const frame = document.querySelector('.hollow-frame');
          const title = document.querySelector('.title-label');
          const close = document.querySelector('.close-button');
          const share = document.querySelector('[data-region-share-control]');
          const titleBar = document.querySelector('.title-bar');
          const frameRect = frame?.getBoundingClientRect();
          const titleRect = title?.getBoundingClientRect();
          const closeRect = close?.getBoundingClientRect();
          const shareRect = share?.getBoundingClientRect();
          const titleBarRect = titleBar?.getBoundingClientRect();
          return {
            frameRight: frameRect?.right ?? 0,
            titleHeight: titleBarRect?.height ?? 0,
            titleBottom: titleBarRect?.bottom ?? 0,
            closeRight: closeRect?.right ?? 0,
            shareWidth: shareRect?.width ?? 0,
            shareHeight: shareRect?.height ?? 0,
            shareLeft: shareRect?.left ?? 0,
            shareRight: shareRect?.right ?? 0,
            shareTop: shareRect?.top ?? 0,
            shareBottom: shareRect?.bottom ?? 0,
            shareLabel: share?.getAttribute('aria-label') ?? null,
            shareTitle: share?.getAttribute('title') ?? null,
            shareScrollWidth: share?.scrollWidth ?? 0,
            shareClientWidth: share?.clientWidth ?? 0,
            labelText: title?.textContent ?? null,
            labelScrollWidth: title?.scrollWidth ?? 0,
            labelClientWidth: title?.clientWidth ?? 0,
            titleBarScrollWidth: titleBar?.scrollWidth ?? 0,
            titleBarClientWidth: titleBar?.clientWidth ?? 0,
            documentScrollWidth: document.documentElement.scrollWidth,
            documentClientWidth: document.documentElement.clientWidth
          };
        })()`
      )) as {
        frameRight: number;
        titleHeight: number;
        titleBottom: number;
        closeRight: number;
        shareWidth: number;
        shareHeight: number;
        shareLeft: number;
        shareRight: number;
        shareTop: number;
        shareBottom: number;
        shareLabel: string | null;
        shareTitle: string | null;
        shareScrollWidth: number;
        shareClientWidth: number;
        labelText: string | null;
        labelScrollWidth: number;
        labelClientWidth: number;
        titleBarScrollWidth: number;
        titleBarClientWidth: number;
        documentScrollWidth: number;
        documentClientWidth: number;
      };
      assert.ok(compact.titleHeight > 44, `${scenario.name}/compact long title did not grow the header`);
      assert.ok(compact.titleBottom > 49, `${scenario.name}/compact title boundary did not move with the wrapped header`);
      assert.equal(compact.labelText, compactTitle, `${scenario.name}/compact long title was truncated`);
      assert.ok(
        compact.labelScrollWidth <= compact.labelClientWidth + 1,
        `${scenario.name}/compact long title overflowed its wrapping label`
      );
      assert.equal(compact.shareWidth, 40, `${scenario.name}/compact Share control width drifted`);
      assert.equal(compact.shareHeight, 40, `${scenario.name}/compact Share control height drifted`);
      assert.ok(
        compact.shareLeft >= 6 - 1 && compact.shareRight <= compact.frameRight - 6 + 1,
        `${scenario.name}/compact Share control escaped the frame: ${JSON.stringify(compact)}`
      );
      assert.ok(
        compact.shareTop >= 6 - 1 && compact.shareBottom <= compact.titleBottom + 1,
        `${scenario.name}/compact Share control escaped the title bar: ${JSON.stringify(compact)}`
      );
      assert.match(compact.shareLabel ?? '', /^(Share|Stop)/, `${scenario.name}/compact Share control is unnamed`);
      assert.ok(compact.shareTitle, `${scenario.name}/compact Share control has no tooltip`);
      assert.ok(
        compact.shareScrollWidth <= compact.shareClientWidth + 1,
        `${scenario.name}/compact Share control has overflowing content`
      );
      assert.ok(
        compact.titleBarScrollWidth <= compact.titleBarClientWidth + 1,
        `${scenario.name}/compact title bar overflowed horizontally`
      );
      assert.ok(
        compact.closeRight <= compact.frameRight - 13 + 1,
        `${scenario.name}/compact close control lost its inner-ring clearance`
      );
      assert.ok(
        compact.documentScrollWidth <= compact.documentClientWidth + 1,
        `${scenario.name}/compact region chrome overflows its viewport`
      );
      await browser.call('Target.closeTarget', { targetId });
    }

    const { targetId: probeTargetId } = (await browser.call('Target.createTarget', {
      url: 'about:blank',
      width: 640,
      height: 400
    })) as { targetId: string };
    const { sessionId: probeSessionId } = (await browser.call('Target.attachToTarget', {
      targetId: probeTargetId,
      flatten: true
    })) as { sessionId: string };
    await browser.call(
      'Emulation.setDeviceMetricsOverride',
      {
        width: 640,
        height: 400,
        deviceScaleFactor: 1,
        mobile: false,
        screenWidth: 640,
        screenHeight: 400,
        dontSetVisibleSize: false
      },
      probeSessionId
    );
    await browser.call(
      'Page.navigate',
      { url: `${fixtureUrl}?ipcProbe=1&ipcDelayMs=260` },
      probeSessionId
    );
    const probeDeadline = Date.now() + 10_000;
    let probeRendered = false;
    while (Date.now() < probeDeadline) {
      const state = (await browser.evaluate(
        probeSessionId,
        `({ rendered: document.body?.dataset.regionRendered ?? null, error: document.body?.dataset.regionRenderedError ?? null })`
      )) as { rendered?: string | null; error?: string | null } | null;
      if (state?.error) {
        throw new Error(`region IPC probe fixture failed: ${decodeURIComponent(state.error)}`);
      }
      if (state?.rendered) {
        probeRendered = true;
        break;
      }
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    assert.ok(probeRendered, 'region IPC probe fixture did not render before the deadline');
    await browser.evaluate(
      probeSessionId,
      'new Promise((resolve) => setTimeout(resolve, 650))'
    );
    const probe = (await browser.evaluate(
      probeSessionId,
      `(() => {
        const probe = window.__regionIpcProbe;
        return {
          maxPollBatchesInFlight: probe?.maxPollBatchesInFlight ?? 0,
          maxInFlight: probe?.maxInFlight ?? 0,
          staleApplyCount: probe?.staleApplyCount ?? 0,
          appliedIgnoreStates: probe?.appliedIgnoreStates ?? [],
          latestDesiredIgnoreState: probe?.latestDesiredIgnoreState ?? null
        };
      })()`
    )) as {
      maxPollBatchesInFlight: number;
      maxInFlight: number;
      staleApplyCount: number;
      appliedIgnoreStates: boolean[];
      latestDesiredIgnoreState: boolean | null;
    };
    assert.equal(
      probe.maxPollBatchesInFlight,
      1,
      `delayed selector polls overlapped: ${probe.maxPollBatchesInFlight} batches in flight`
    );
    assert.ok(
      probe.maxInFlight <= 3,
      `delayed selector poll exceeded one 3-call batch: ${probe.maxInFlight} native calls in flight`
    );
    assert.equal(
      probe.staleApplyCount,
      0,
      `a stale delayed poll applied ${JSON.stringify(probe.appliedIgnoreStates)} after the newest desired state ${probe.latestDesiredIgnoreState}`
    );

    await browser.call('Target.closeTarget', { targetId: probeTargetId });

    const { targetId: eventTargetId } = (await browser.call('Target.createTarget', {
      url: 'about:blank',
      width: 640,
      height: 400
    })) as { targetId: string };
    const { sessionId: eventSessionId } = (await browser.call('Target.attachToTarget', {
      targetId: eventTargetId,
      flatten: true
    })) as { sessionId: string };
    await browser.call(
      'Emulation.setDeviceMetricsOverride',
      {
        width: 640,
        height: 400,
        deviceScaleFactor: 1,
        mobile: false,
        screenWidth: 640,
        screenHeight: 400,
        dontSetVisibleSize: false
      },
      eventSessionId
    );
    await browser.call(
      'Page.navigate',
      { url: `${fixtureUrl}?ipcProbe=1&ipcDelayMs=5&cursorX=320&cursorY=200` },
      eventSessionId
    );
    const eventProbeDeadline = Date.now() + 10_000;
    let eventProbeRendered = false;
    while (Date.now() < eventProbeDeadline) {
      const state = (await browser.evaluate(
        eventSessionId,
        `({ rendered: document.body?.dataset.regionRendered ?? null, error: document.body?.dataset.regionRenderedError ?? null })`
      )) as { rendered?: string | null; error?: string | null } | null;
      if (state?.error) {
        throw new Error(`region event probe fixture failed: ${decodeURIComponent(state.error)}`);
      }
      if (state?.rendered) {
        eventProbeRendered = true;
        break;
      }
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    assert.ok(eventProbeRendered, 'region event probe fixture did not render before the deadline');
    await new Promise((resolve) => setTimeout(resolve, 180));
    const initialEventState = (await browser.evaluate(
      eventSessionId,
      'window.__regionIpcProbe.appliedIgnoreStates'
    )) as boolean[];
    assert.deepEqual(initialEventState, [true], 'event probe did not establish the initial center state');
    // Wait for each ignore-state transition to actually land before driving
    // the next geometry event. A fixed sleep raced the applier: the states are
    // debounced, so two events inside one debounce window coalesce into a
    // single applied transition and the exact-sequence assertion below fails
    // (~50% of runs). Convergence was always correct -- the final applied
    // state matched `latestDesiredIgnoreState` even on failures -- so this is
    // a sampling race, not the hit-test defect it looked like. Waiting keeps
    // the assertion EXACT rather than loosening it to "converged". (#903)
    const awaitAppliedTransitions = async (count: number) => {
      await browser.evaluate(
        eventSessionId,
        `(async () => {
          const deadline = Date.now() + 4000;
          while (Date.now() < deadline) {
            if ((window.__regionIpcProbe?.appliedIgnoreStates?.length ?? 0) >= ${count}) return;
            await new Promise((r) => setTimeout(r, 16));
          }
        })()`
      );
    };
    await browser.evaluate(
      eventSessionId,
      `(() => {
        const probe = window.__regionIpcProbe;
        probe.resetTransitions();
        probe.setGeometry({ x: 319, y: 0 }, { width: 640, height: 400 });
      })()`
    );
    await awaitAppliedTransitions(1);
    await browser.evaluate(
      eventSessionId,
      `window.__regionIpcProbe.setGeometry({ x: 0, y: 0 }, { width: 640, height: 400 })`
    );
    await awaitAppliedTransitions(2);
    await browser.evaluate(
      eventSessionId,
      `window.__regionIpcProbe.setGeometry(null, { width: 320, height: 200 })`
    );
    await awaitAppliedTransitions(3);
    await browser.evaluate(
      eventSessionId,
      `window.__regionIpcProbe.setScale(2, { width: 640, height: 400 })`
    );
    await awaitAppliedTransitions(4);
    const eventDriven = (await browser.evaluate(
      eventSessionId,
      `(() => {
        const probe = window.__regionIpcProbe;
        return {
          appliedIgnoreStates: probe.appliedIgnoreStates,
          eventCounts: probe.eventCounts,
          eventDeliveries: probe.eventDeliveries,
          cursorHistory: probe.cursorHistory,
          pollCalls: probe.pollCalls,
          latestDesiredIgnoreState: probe.latestDesiredIgnoreState,
          listenerCount: probe.listenerCount
        };
      })()`
    )) as {
      appliedIgnoreStates: boolean[];
      eventCounts: Record<string, number>;
      eventDeliveries: Record<string, number>;
      cursorHistory: Array<{ x: number; y: number }>;
      pollCalls: number;
      latestDesiredIgnoreState: boolean | null;
      listenerCount: number;
    };
    assert.deepEqual(
      eventDriven.appliedIgnoreStates,
      [false, true, false, true],
      `move/resize/scale events did not update the cached hit-test geometry: ${JSON.stringify(eventDriven)}`
    );
    assert.ok(
      (eventDriven.eventCounts['tauri://move'] ?? 0) >= 3 &&
        (eventDriven.eventCounts['tauri://resize'] ?? 0) >= 3 &&
        (eventDriven.eventCounts['tauri://scale-change'] ?? 0) >= 1,
      `geometry events were not delivered to the selector: ${JSON.stringify(eventDriven.eventCounts)}`
    );
    assert.ok(
      (eventDriven.eventDeliveries['tauri://move'] ?? 0) >= 3 &&
        (eventDriven.eventDeliveries['tauri://resize'] ?? 0) >= 3 &&
        (eventDriven.eventDeliveries['tauri://scale-change'] ?? 0) >= 1,
      `selector did not consume geometry events: ${JSON.stringify(eventDriven.eventDeliveries)}`
    );
    assert.equal(eventDriven.latestDesiredIgnoreState, true);
    assert.ok(eventDriven.listenerCount >= 4, 'selector did not register native geometry listeners');

    const callsBeforeUnmount = eventDriven.pollCalls;
    await browser.evaluate(eventSessionId, 'window.__unmountRegion?.()');
    await new Promise((resolve) => setTimeout(resolve, 180));
    const afterUnmount = (await browser.evaluate(
      eventSessionId,
      `(() => ({
        pollCalls: window.__regionIpcProbe.pollCalls,
        pollCallsAfterUnmount: window.__regionIpcProbe.pollCallsAfterUnmount,
        listenerCount: window.__regionIpcProbe.listenerCount
      }))()`
    )) as { pollCalls: number; pollCallsAfterUnmount: number; listenerCount: number };
    assert.ok(
      afterUnmount.pollCalls <= callsBeforeUnmount + 1,
      'selector polling continued after teardown'
    );
    assert.equal(afterUnmount.pollCallsAfterUnmount, 0, 'selector started a poll after teardown');
    assert.equal(afterUnmount.listenerCount, 0, 'selector native listeners survived teardown');
    await browser.call('Target.closeTarget', { targetId: eventTargetId });

    const { targetId: placementTargetId } = (await browser.call('Target.createTarget', {
      url: 'about:blank',
      width: 640,
      height: 400
    })) as { targetId: string };
    const { sessionId: placementSessionId } = (await browser.call('Target.attachToTarget', {
      targetId: placementTargetId,
      flatten: true
    })) as { sessionId: string };
    await browser.call(
      'Emulation.setDeviceMetricsOverride',
      {
        width: 640,
        height: 400,
        deviceScaleFactor: 1,
        mobile: false,
        screenWidth: 640,
        screenHeight: 400,
        dontSetVisibleSize: false
      },
      placementSessionId
    );
    await browser.call(
      'Page.navigate',
      { url: `${fixtureUrl}?ipcProbe=1&placing=1&ipcDelayMs=5&cursorX=320&cursorY=200` },
      placementSessionId
    );
    const placementDeadline = Date.now() + 10_000;
    let placementRendered = false;
    while (Date.now() < placementDeadline) {
      const state = (await browser.evaluate(
        placementSessionId,
        `({ rendered: document.body?.dataset.regionRendered ?? null, error: document.body?.dataset.regionRenderedError ?? null })`
      )) as { rendered?: string | null; error?: string | null } | null;
      if (state?.error) {
        throw new Error(`region placement probe fixture failed: ${decodeURIComponent(state.error)}`);
      }
      if (state?.rendered) {
        placementRendered = true;
        break;
      }
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    assert.ok(placementRendered, 'region placement probe fixture did not render before the deadline');
    await new Promise((resolve) => setTimeout(resolve, 180));
    const beforePlacementSettlement = (await browser.evaluate(
      placementSessionId,
      `(() => {
        const probe = window.__regionIpcProbe;
        return {
          placementActive: probe?.placementActive ?? false,
          placementSettled: probe?.placementSettled ?? false,
          earlyClickThroughRequests: probe?.earlyClickThroughRequests ?? 0,
          appliedIgnoreStates: probe?.appliedIgnoreStates ?? []
        };
      })()`
    )) as {
      placementActive: boolean;
      placementSettled: boolean;
      earlyClickThroughRequests: number;
      appliedIgnoreStates: boolean[];
    };
    assert.equal(beforePlacementSettlement.placementActive, true, 'placement probe did not start in placement mode');
    assert.equal(beforePlacementSettlement.placementSettled, false, 'placement probe settled before its event');
    assert.equal(
      beforePlacementSettlement.earlyClickThroughRequests,
      0,
      `placement enabled click-through before settlement: ${JSON.stringify(beforePlacementSettlement)}`
    );
    assert.ok(
      beforePlacementSettlement.appliedIgnoreStates.every((state) => !state),
      `placement applied click-through before settlement: ${JSON.stringify(beforePlacementSettlement.appliedIgnoreStates)}`
    );
    await browser.evaluate(
      placementSessionId,
      `window.__regionIpcProbe.settlePlacement('region-window-999')`
    );
    await new Promise((resolve) => setTimeout(resolve, 120));
    const afterStalePlacementEvent = (await browser.evaluate(
      placementSessionId,
      `(() => {
        const probe = window.__regionIpcProbe;
        return {
          placementSettled: probe?.placementSettled ?? false,
          earlyClickThroughRequests: probe?.earlyClickThroughRequests ?? 0,
          appliedIgnoreStates: probe?.appliedIgnoreStates ?? [],
          clickThroughRequests: probe?.clickThroughRequests ?? [],
          placementSettlementLabels: probe?.placementSettlementLabels ?? [],
          commandHistory: probe?.commandHistory ?? [],
          routePlacementActive: document.querySelector('.window-container')?.getAttribute('data-placement-active'),
          routePlacementPending: document.querySelector('.window-container')?.getAttribute('data-placement-settlement-pending')
        };
      })()`
    )) as {
      placementSettled: boolean;
      earlyClickThroughRequests: number;
      appliedIgnoreStates: boolean[];
      clickThroughRequests: Array<{ applied: boolean; beforePlacementSettlement: boolean }>;
      placementSettlementLabels: Array<string | null>;
      commandHistory: string[];
      routePlacementActive: string | null;
      routePlacementPending: string | null;
    };
    assert.equal(afterStalePlacementEvent.placementSettled, false, 'a stale selector event settled this route');
    assert.ok(
      afterStalePlacementEvent.appliedIgnoreStates.every((state) => !state),
      `stale selector event enabled click-through: ${JSON.stringify(afterStalePlacementEvent)}`
    );
    await browser.evaluate(
      placementSessionId,
      `document.dispatchEvent(new MouseEvent('mousedown', { bubbles: true, button: 0 }))`
    );
    await browser.evaluate(
      placementSessionId,
      `window.__regionIpcProbe.settlePlacement()`
    );
    const afterSettlementBeforeMouseup = (await browser.evaluate(
      placementSessionId,
      `(() => ({
        placementSettled: window.__regionIpcProbe.placementSettled,
        earlyClickThroughRequests: window.__regionIpcProbe.earlyClickThroughRequests,
        clickThroughRequests: window.__regionIpcProbe.clickThroughRequests,
        routePlacementActive: document.querySelector('.window-container')?.getAttribute('data-placement-active'),
        routePlacementPending: document.querySelector('.window-container')?.getAttribute('data-placement-settlement-pending')
      }))()`
    )) as {
      placementSettled: boolean;
      earlyClickThroughRequests: number;
      clickThroughRequests: Array<{ applied: boolean; beforePlacementSettlement: boolean }>;
      routePlacementActive: string | null;
      routePlacementPending: string | null;
    };
    assert.equal(afterSettlementBeforeMouseup.placementSettled, true);
    assert.equal(afterSettlementBeforeMouseup.earlyClickThroughRequests, 0);
    assert.equal(afterSettlementBeforeMouseup.routePlacementActive, 'false');
    assert.equal(afterSettlementBeforeMouseup.routePlacementPending, 'true');
    await browser.evaluate(
      placementSessionId,
      `document.dispatchEvent(new MouseEvent('mouseup', { bubbles: true, button: 0 }))`
    );
    await browser.evaluate(
      placementSessionId,
      `window.__regionIpcProbe.releasePlacement()`
    );
    await new Promise((resolve) => setTimeout(resolve, 180));
    const afterPlacementSettlement = (await browser.evaluate(
      placementSessionId,
      `(() => {
        const probe = window.__regionIpcProbe;
        return {
          placementSettled: probe?.placementSettled ?? false,
          earlyClickThroughRequests: probe?.earlyClickThroughRequests ?? 0,
          appliedIgnoreStates: probe?.appliedIgnoreStates ?? [],
          clickThroughRequests: probe?.clickThroughRequests ?? [],
          placementSettlementLabels: probe?.placementSettlementLabels ?? [],
          commandHistory: probe?.commandHistory ?? [],
          routePlacementActive: document.querySelector('.window-container')?.getAttribute('data-placement-active'),
          routePlacementPending: document.querySelector('.window-container')?.getAttribute('data-placement-settlement-pending')
        };
      })()`
    )) as {
      placementSettled: boolean;
      earlyClickThroughRequests: number;
      appliedIgnoreStates: boolean[];
      clickThroughRequests: Array<{ applied: boolean; beforePlacementSettlement: boolean }>;
      placementSettlementLabels: Array<string | null>;
      commandHistory: string[];
      routePlacementActive: string | null;
      routePlacementPending: string | null;
    };
    assert.equal(afterPlacementSettlement.placementSettled, true, 'placement settlement event was not observed');
    assert.equal(
      afterPlacementSettlement.earlyClickThroughRequests,
      0,
      `click-through was requested before placement settlement: before=${JSON.stringify(beforePlacementSettlement)} stale=${JSON.stringify(afterStalePlacementEvent)} after=${JSON.stringify(afterPlacementSettlement)}`
    );
    assert.ok(
      afterPlacementSettlement.appliedIgnoreStates.includes(true),
      `placement never restored dynamic click-through after settlement: ${JSON.stringify(afterPlacementSettlement)}`
    );
    await browser.call('Target.closeTarget', { targetId: placementTargetId });
  } finally {
    try {
      await browser?.close();
    } finally {
      await Promise.all([
        removeTempPath(buildDir),
        removeTempPath(profileDir)
      ]);
    }
  }
});
