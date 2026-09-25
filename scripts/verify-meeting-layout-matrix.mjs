#!/usr/bin/env node
// Meeting layout device matrix (#239 step 0).
//
// Drives the REAL browser client against a local LiveKit server with N
// headless peers (fake cameras; one can share the test pattern) and a viewer
// under phone / tablet / desktop emulation, in portrait and landscape, with
// the browser toolbar showing and in full screen. For every cell it saves a
// screenshot and records, from real rendered geometry:
//   - the fraction of the viewport the tiles, and their video, cover;
//   - whether every participant is reachable (visible, or one scroll of the
//     strip / grid away) and, in spotlight, that your own thumbnail is no
//     smaller than anyone else's;
//   - controls clipped off screen or overlapping, and whether Mic, Camera and
//     Leave are all on screen; a control the scroller cuts must not show a
//     half-label;
//   - on a portrait phone, that the top bar stays one slim row (<= 52px, the
//     room name on one line), also with the keyboard up (the chat composer
//     focused, the viewport cut to 82% of its width: short, but no rail);
//   - that the page itself never scrolls, and that on a phone the developer
//     & test tools take no screen (desktop keeps them as a bottom row);
//   - with a shared window as hero, that its video is the biggest picture;
//   - camera tiles still waiting for video (a load symptom, not a layout one:
//     compare before and after runs rather than judging a single cell).
// With --check it exits non-zero when a cell misses #239's definition of done.
//
// Prerequisites (all local, no prod dependencies), as for
// scripts/verify-receiver-render.mjs:
//   livekit-server --dev                          # ws://localhost:7880
//   apps/desktop/.env with LIVEKIT_URL=ws://localhost:7880, devkey/secret
//   (cd web-harness && npx vite --port 5199)      # or set PETAL_WEB_URL
//
// Run:  node scripts/verify-meeting-layout-matrix.mjs --out /tmp/matrix [--check]
//         [--devices pixel-8-l,iphone-se-p,...] [--counts 1,2,4,7] [--no-share] [--no-chat]
//         [--cameras-off N]   (the first N peers join with their camera off)
//       node scripts/verify-meeting-layout-matrix.mjs --out /tmp/matrix --recheck   (re-judge metrics.json)
// Every iPhone row runs with `document.fullscreenEnabled` forced false, as on
// iPhone Safari; the browser is Chromium throughout, so Safari-only rendering
// still needs a real device.

import { createRequire } from 'node:module';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

const repoRoot = resolve(import.meta.dirname, '..');
const baseUrl = (process.env.PETAL_WEB_URL ?? 'http://localhost:5199').replace(/\/$/, '');

let chromium;
let playwrightDevices;
try {
  const playwrightModule =
    process.env.PETAL_PLAYWRIGHT_MODULE ?? resolve(repoRoot, 'apps/desktop/node_modules/playwright');
  ({ chromium, devices: playwrightDevices } = createRequire(import.meta.url)(playwrightModule));
} catch (error) {
  console.error(`Playwright unavailable: ${error instanceof Error ? error.message : error}`);
  process.exit(2);
}

function arg(name, fallback) {
  const index = process.argv.indexOf(`--${name}`);
  if (index < 0) return fallback;
  const value = process.argv[index + 1];
  return value === undefined || value.startsWith('--') ? true : value;
}

// ---------------------------------------------------------------------------
// Devices. Playwright's viewports already subtract the browser toolbar; the
// `-fs` rows use the device's whole screen, i.e. after the full-screen button.
// ---------------------------------------------------------------------------
function playwrightDevice(label, name, overrides = {}) {
  const device = playwrightDevices[name];
  if (!device) throw new Error(`unknown Playwright device ${name}`);
  const { defaultBrowserType: _ignored, ...options } = device;
  return { label, ...options, ...overrides };
}

const DEVICES = {
  'iphone-se-p': playwrightDevice('iPhone SE portrait', 'iPhone SE (3rd gen)'),
  'iphone-se-l': playwrightDevice('iPhone SE landscape', 'iPhone SE (3rd gen) landscape'),
  'iphone-15-p': playwrightDevice('iPhone 15 portrait', 'iPhone 15'),
  'iphone-15-l': playwrightDevice('iPhone 15 landscape', 'iPhone 15 landscape'),
  'iphone-15pm-p': playwrightDevice('iPhone 15 Pro Max portrait', 'iPhone 15 Pro Max'),
  'iphone-15pm-l': playwrightDevice('iPhone 15 Pro Max landscape', 'iPhone 15 Pro Max landscape'),
  'galaxy-s24-p': playwrightDevice('Galaxy S24 portrait', 'Galaxy S24'),
  'galaxy-s24-l': playwrightDevice('Galaxy S24 landscape', 'Galaxy S24 landscape'),
  'pixel-8-p': playwrightDevice('Pixel 8 portrait', 'Pixel 8'),
  'pixel-8-l': playwrightDevice('Pixel 8 landscape', 'Pixel 8 landscape'),
  'pixel-8-p-fs': playwrightDevice('Pixel 8 portrait, full screen', 'Pixel 8', { viewport: { width: 412, height: 915 } }),
  'pixel-8-l-fs': playwrightDevice('Pixel 8 landscape, full screen', 'Pixel 8 landscape', { viewport: { width: 915, height: 412 } }),
  'ipad-mini-p': playwrightDevice('iPad mini portrait', 'iPad Mini'),
  'ipad-mini-l': playwrightDevice('iPad mini landscape', 'iPad Mini landscape'),
  'desktop-1280': { label: 'Desktop 1280x800', viewport: { width: 1280, height: 800 }, deviceScaleFactor: 1, isMobile: false, hasTouch: false },
  'desktop-1920': { label: 'Desktop 1920x1080', viewport: { width: 1920, height: 1080 }, deviceScaleFactor: 1, isMobile: false, hasTouch: false },
};
const LANDSCAPE_PHONES = ['iphone-se-l', 'iphone-15-l', 'iphone-15pm-l', 'galaxy-s24-l', 'pixel-8-l', 'pixel-8-l-fs'];
const PORTRAIT_PHONES = ['iphone-se-p', 'iphone-15-p', 'iphone-15pm-p', 'galaxy-s24-p', 'pixel-8-p', 'pixel-8-p-fs'];

const deviceKeys = String(arg('devices', Object.keys(DEVICES).join(','))).split(',');
const counts = String(arg('counts', '1,2,4,7')).split(',').map(Number);
const withShare = !arg('no-share', false);
const withChat = !arg('no-chat', false);
const camerasOff = Number.parseInt(String(arg('cameras-off', '0')), 10);
if (!Number.isInteger(camerasOff) || camerasOff < 0) {
  console.error('--cameras-off takes a whole number of peers, e.g. --cameras-off 2');
  process.exit(2);
}
const check = Boolean(arg('check', false));
const outDir = resolve(String(arg('out', join(process.cwd(), 'meeting-layout-matrix'))));
mkdirSync(outDir, { recursive: true });

const PEER_NAMES = ['Alice', 'Bob', 'Carol', 'Dave', 'Erin', 'Frank', 'Grace', 'Heidi'];
const roomCode = () => Array.from({ length: 10 }, () => 'abcdefghjkmnopqrstuvwxyz'[Math.floor(Math.random() * 24)]).join('');

// Telemetry never leaves a test run, and neither does a token request: a
// production build without VITE_PETAL_BACKEND_URL would ask app.petal.live.
const TELEMETRY = /sentry\.io|ingest\.(us\.)?sentry|posthog|petal\.live/i;
async function newContext(browser, options) {
  const context = await browser.newContext({ ...options, permissions: ['camera', 'microphone'] });
  await context.route((url) => TELEMETRY.test(url.toString()), (route) => route.abort());
  return context;
}

async function joinMeeting(page, code, name, camera = true) {
  await page.goto(`${baseUrl}/`, { waitUntil: 'domcontentloaded' });
  await page.waitForFunction(() => window.__petalHarness?.cockpitAutoScenario?.join, null, { timeout: 20_000 });
  await page.evaluate(
    ([meetingCode, displayName]) => {
      const input = document.querySelector('#display-name');
      if (input) input.value = displayName;
      return window.__petalHarness.cockpitAutoScenario.join(meetingCode);
    },
    [code, name]
  );
  await page.waitForFunction(() => window.__petalHarness?.room?.state === 'connected', null, { timeout: 30_000 });
  if (camera) await page.evaluate(() => document.querySelector('#ctl-video')?.click());
}

async function spawnPeers(browser, code, count) {
  const peers = [];
  for (let index = 0; index < count; index += 1) {
    const context = await newContext(browser, { viewport: { width: 640, height: 480 } });
    const page = await context.newPage();
    await joinMeeting(page, code, PEER_NAMES[index] ?? `Peer ${index + 1}`, index >= camerasOff);
    peers.push({ context, page });
  }
  return peers;
}

async function openViewer(browser, code, key) {
  const { label: _label, ...options } = DEVICES[key];
  const context = await newContext(browser, options);
  if (/iPhone/.test(options.userAgent ?? '')) {
    // iPhone Safari has no element full screen: the button must not exist.
    await context.addInitScript(() => Object.defineProperty(Document.prototype, 'fullscreenEnabled', { get: () => false }));
  }
  const page = await context.newPage();
  page.on('pageerror', (error) => console.error(`[${key} pageerror] ${error.message}`));
  await joinMeeting(page, code, 'Me');
  // A deployed build shows the bug-report trigger in the top bar; a local
  // one has no UserDispatch key, so show it to measure the real top bar.
  await page.evaluate(() => document.querySelector('#feedback-meeting-trigger')?.removeAttribute('hidden'));
  await page.waitForTimeout(2500); // subscribe + first frames
  return { context, page };
}

async function leave({ context, page }) {
  await page.evaluate(() => window.__petalHarness?.room?.disconnect?.()).catch(() => {});
  await context.close().catch(() => {});
}

async function setLayout(page, mode) {
  await page.evaluate((wanted) => {
    const label = wanted === 'grid' ? 'Grid view' : 'Spotlight view';
    document.querySelector(`.layout-mode-button[aria-label="${label}"]`)?.click();
  }, mode);
  await page.waitForTimeout(700);
}

/** The landscape rail layout (#239) is the one where the top bar floats. */
function railLayout(page) {
  return page.evaluate(() => getComputedStyle(document.querySelector('#meeting-screen')).display === 'grid');
}

/** The landscape top bar fades after a few idle seconds; shoot the settled state. */
async function settle(page) {
  if (await railLayout(page)) await page.waitForSelector('#meeting-screen.chrome-idle', { timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(300);
}

// ---------------------------------------------------------------------------
// Metrics, from rendered geometry.
// ---------------------------------------------------------------------------
async function measure(page) {
  return page.evaluate(() => {
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    const area = (r) => Math.max(0, r.right - r.left) * Math.max(0, r.bottom - r.top);
    const clip = (r, box = { left: 0, top: 0, right: vw, bottom: vh }) => ({
      left: Math.max(r.left, box.left),
      top: Math.max(r.top, box.top),
      right: Math.min(r.right, box.right),
      bottom: Math.min(r.bottom, box.bottom),
    });
    const shown = (el) => {
      const style = getComputedStyle(el);
      const r = el.getBoundingClientRect();
      return style.display !== 'none' && style.visibility !== 'hidden' && r.width > 0 && r.height > 0;
    };
    const inside = (r, box, slack = 1) =>
      r.left >= box.left - slack && r.top >= box.top - slack && r.right <= box.right + slack && r.bottom <= box.bottom + slack;
    const scroller = (el) => {
      for (let node = el.parentElement; node; node = node.parentElement) {
        const style = getComputedStyle(node);
        if (/(auto|scroll)/.test(style.overflowX + style.overflowY) && node !== document.scrollingElement && node !== document.body) {
          return node;
        }
      }
      return null;
    };
    // Rendered video content inside a tile (object-fit aware).
    function videoRect(video) {
      const r = video.getBoundingClientRect();
      if (getComputedStyle(video).objectFit !== 'contain' || !video.videoWidth) return r;
      const scale = Math.min(r.width / video.videoWidth, r.height / video.videoHeight);
      const w = video.videoWidth * scale;
      const h = video.videoHeight * scale;
      return { left: r.left + (r.width - w) / 2, top: r.top + (r.height - h) / 2, right: r.left + (r.width + w) / 2, bottom: r.top + (r.height + h) / 2 };
    }

    const localIdentity = window.__petalHarness?.room?.localParticipant?.identity;
    const tiles = [...document.querySelectorAll('#tiles .tile')].filter(shown);
    let tileArea = 0;
    let videoArea = 0;
    const participants = [];
    for (const tile of tiles) {
      const r = tile.getBoundingClientRect();
      const box = scroller(tile)?.getBoundingClientRect() ?? { left: 0, top: 0, right: vw, bottom: vh };
      const visible = clip(clip(r), box);
      tileArea += area(visible);
      const video = tile.querySelector('video.camera-video-ready, video.share-video');
      const tileVideoArea = video && shown(video) ? area(clip(clip(videoRect(video)), box)) : 0;
      videoArea += tileVideoArea;
      const anyVideo = tile.querySelector('video');
      participants.push({
        media: tile.classList.contains('camera-off')
          ? 'camera-off'
          : anyVideo && anyVideo.videoWidth > 0 && (video || anyVideo.readyState >= 2)
            ? 'video'
            : anyVideo
              ? 'waiting'
              : 'none',
        videoArea: Math.round(tileVideoArea),
        name: tile.querySelector('.name-chip-label')?.textContent?.trim() || tile.dataset.owner,
        isShare: tile.classList.contains('share-tile'),
        isLocal: tile.dataset.owner === localIdentity && !tile.classList.contains('share-tile'),
        isHero: tile.classList.contains('is-spotlight'),
        isThumbnail: tile.classList.contains('is-spotlight-thumbnail'),
        w: Math.round(r.width),
        h: Math.round(r.height),
        visibleNow: inside(r, { left: 0, top: 0, right: vw, bottom: vh }) && inside(r, box),
      });
    }

    // Reachability: scroll each hidden tile's own scroller to it and look again.
    const reachable = tiles.map((tile) => {
      const r0 = tile.getBoundingClientRect();
      const box0 = scroller(tile)?.getBoundingClientRect() ?? { left: 0, top: 0, right: vw, bottom: vh };
      if (inside(r0, { left: 0, top: 0, right: vw, bottom: vh }) && inside(r0, box0)) return true;
      const container = scroller(tile);
      if (!container) return false;
      const before = [container.scrollLeft, container.scrollTop];
      const c = container.getBoundingClientRect();
      container.scrollLeft += r0.left - c.left - (c.width - r0.width) / 2;
      container.scrollTop += r0.top - c.top - (c.height - r0.height) / 2;
      const r1 = tile.getBoundingClientRect();
      const ok = inside(r1, container.getBoundingClientRect()) && inside(r1, { left: 0, top: 0, right: vw, bottom: vh });
      [container.scrollLeft, container.scrollTop] = before;
      return ok;
    });

    // Controls: on screen, pinned or one scroll away, never overlapping.
    const controls = [...document.querySelectorAll('.controlbar button, .topbar button')].filter(shown);
    const controlInfo = controls.map((button) => {
      const r = button.getBoundingClientRect();
      const container = scroller(button);
      const box = container?.getBoundingClientRect();
      return {
        id: button.id || button.getAttribute('aria-label') || button.className,
        r,
        scrolledAway: Boolean(box) && !inside(r, box),
        offScreen: !inside(r, { left: 0, top: 0, right: vw, bottom: vh }),
        covered: getComputedStyle(button.closest('.topbar') ?? button).opacity === '0',
      };
    });
    const onScreen = controlInfo.filter((c) => !c.scrolledAway && !c.covered);
    const overlaps = [];
    for (let i = 0; i < onScreen.length; i += 1) {
      for (let j = i + 1; j < onScreen.length; j += 1) {
        const a = onScreen[i].r;
        const b = onScreen[j].r;
        const x = Math.min(a.right, b.right) - Math.max(a.left, b.left);
        const y = Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top);
        if (x > 1 && y > 1) overlaps.push(`${onScreen[i].id} / ${onScreen[j].id}`);
      }
    }
    const essential = ['ctl-audio', 'ctl-video', 'ctl-leave'].filter((id) => {
      const info = controlInfo.find((c) => c.id === id);
      return !info || info.scrolledAway || info.offScreen;
    });

    const hero = participants.find((p) => p.isHero);
    const thumbnails = participants.filter((p) => p.isThumbnail);
    const self = thumbnails.find((p) => p.isLocal);
    const others = thumbnails.filter((p) => !p.isLocal);
    const devPanel = document.querySelector('#dev-panel')?.getBoundingClientRect();
    // Does the document itself scroll? Try it, then put it back.
    const scrollBefore = window.scrollY;
    window.scrollTo(0, scrollBefore + 200);
    const documentScrolls = window.scrollY !== scrollBefore;
    window.scrollTo(0, scrollBefore);
    const fullscreenButton = [...document.querySelectorAll('#topbar-fullscreen, #ctl-fullscreen')].find(shown);
    const roomName = document.querySelector('#room-name');
    // A control the scroller cuts, still showing its name ("Ir").
    const cutLabels = [...document.querySelectorAll('.controls-left > .control-cell')]
      .filter((cell) => {
        const label = cell.querySelector('.meeting-control-label');
        const r = cell.getBoundingClientRect();
        const box = cell.parentElement.getBoundingClientRect();
        return label && shown(label) && !inside(r, box, 0.5) && area(clip(r, box)) > 0;
      })
      .map((cell) => cell.querySelector('.meeting-control-label').textContent.trim());
    return {
      viewport: `${vw}x${vh}`,
      tiles: tiles.length,
      tileAreaFraction: +(tileArea / (vw * vh)).toFixed(3),
      videoAreaFraction: +(videoArea / (vw * vh)).toFixed(3),
      visibleNow: participants.filter((p) => p.visibleNow).length,
      unreachable: participants.filter((_, i) => !reachable[i]).map((p) => p.name),
      selfThumbnail: self ? `${self.w}x${self.h}` : null,
      selfNotSmaller: !self || others.every((o) => self.w * self.h >= o.w * o.h - 4),
      hero: hero ? `${hero.w}x${hero.h}` : null,
      heroHeight: hero ? hero.h : null,
      controlsOffScreen: controlInfo.filter((c) => c.offScreen && !c.scrolledAway).map((c) => c.id),
      controlsScrolledAway: controlInfo.filter((c) => c.scrolledAway).map((c) => c.id),
      controlOverlaps: overlaps,
      essentialControlsHidden: essential,
      documentScrolls,
      devToolsOnScreen: devPanel ? devPanel.top < vh - 1 && devPanel.bottom > 0 : false,
      heroVideoArea: hero ? hero.videoArea : null,
      largestThumbnailArea: thumbnails.length ? Math.max(...thumbnails.map((p) => p.videoArea || p.w * p.h)) : null,
      camerasWaiting: participants.filter((p) => !p.isShare && p.media === 'waiting').map((p) => p.name),
      fullscreenButton: fullscreenButton ? fullscreenButton.id : null,
      rail: getComputedStyle(document.querySelector('#meeting-screen')).display === 'grid',
      topbarHeight: Math.round(document.querySelector('.topbar').getBoundingClientRect().height),
      roomNameLines:
        roomName && shown(roomName)
          ? Math.round(roomName.getBoundingClientRect().height / parseFloat(getComputedStyle(roomName).lineHeight))
          : 0,
      cutLabels,
      tileList: participants,
    };
  });
}

// ---------------------------------------------------------------------------
// #239's definition of done, per cell.
// ---------------------------------------------------------------------------

/** The most of a `width` x `height` viewport `count` 16:9 tiles could cover
 * with no chrome, padding or gaps at all: the geometric ceiling. Two tiles on
 * a 1.78:1 iPhone SE in landscape cannot pass 50% however little chrome
 * there is, so 55% is judged on the issue's own phone and the rest against
 * this ceiling. */
function bestVideoFraction(count, width, height) {
  let best = 0;
  for (let columns = 1; columns <= count; columns += 1) {
    const rows = Math.ceil(count / columns);
    const tileWidth = Math.min(width / columns, (height / rows) * (16 / 9));
    best = Math.max(best, (count * tileWidth * tileWidth * (9 / 16)) / (width * height));
  }
  return best;
}

function problems(key, scenario, count, m) {
  const out = [];
  if (m.unreachable.length) out.push(`unreachable: ${m.unreachable.join(', ')}`);
  if (!m.selfNotSmaller) out.push(`own thumbnail ${m.selfThumbnail} is smaller than another`);
  if (scenario.includes('spotlight') && count > 1 && !(m.heroHeight > 40)) out.push(`empty spotlight hero (${m.hero})`);
  if (m.controlOverlaps.length) out.push(`overlapping controls: ${m.controlOverlaps.join('; ')}`);
  if (m.controlsOffScreen.length) out.push(`controls off screen: ${m.controlsOffScreen.join(', ')}`);
  if (m.essentialControlsHidden.length) out.push(`not on screen: ${m.essentialControlsHidden.join(', ')}`);
  if (m.cutLabels?.length) out.push(`a cut control shows its label: ${m.cutLabels.join(', ')}`);
  if (m.documentScrolls) out.push('the page scrolls under the meeting');
  if (PORTRAIT_PHONES.includes(key)) {
    if (m.topbarHeight > 52) out.push(`the top bar is ${m.topbarHeight}px tall`);
    if (m.roomNameLines > 1) out.push(`the room name takes ${m.roomNameLines} lines`);
    if (scenario === 'keyboard' && m.rail) out.push('the keyboard turned the portrait bar into the landscape rail');
  }
  // Desktop and tablets keep the developer row (testers use it); a phone parks it.
  if (m.devToolsOnScreen && !/^(desktop|ipad)/.test(key)) out.push('developer tools take phone screen');
  if (scenario === 'share-spotlight' && m.heroVideoArea !== null && m.heroVideoArea < m.largestThumbnailArea) {
    out.push(`shared window's video (${m.heroVideoArea}px²) is smaller than a thumbnail (${m.largestThumbnailArea}px²)`);
  }
  const ceiling = twoPersonCeiling(key, count, scenario, m);
  if (ceiling !== null && m.videoAreaFraction < 0.55 && m.videoAreaFraction < 0.8 * ceiling) {
    out.push(`2-person video covers ${Math.round(m.videoAreaFraction * 100)}%: under 55% and under 80% of the ${Math.round(ceiling * 100)}% ceiling`);
  }
  return out;
}

/** #239's 2-person bar applies to landscape phones in grid: 55% of the
 * screen, or (where 16:9 tiles cannot reach that at all) 80% of the most
 * they could cover. Returns that ceiling for those cells, null for others. */
function twoPersonCeiling(key, count, scenario, m) {
  if (!LANDSCAPE_PHONES.includes(key) || count !== 2 || scenario !== 'grid') return null;
  const [width, height] = m.viewport.split('x').map(Number);
  return bestVideoFraction(2, width, height);
}

// --recheck: re-judge an existing run's metrics.json with the rules above.
if (arg('recheck', false)) {
  const path = join(outDir, 'metrics.json');
  const cells = JSON.parse(readFileSync(path, 'utf8'));
  let failing = 0;
  for (const [name, cell] of Object.entries(cells)) {
    const [key, participants] = name.split('--');
    cell.issues = problems(key, cell.scenario, Number(participants.slice(1)), cell);
    if (cell.issues.length) {
      failing += 1;
      console.log(`FAIL ${name}: ${cell.issues.join(' | ')}`);
    }
  }
  writeFileSync(path, JSON.stringify(cells, null, 2));
  console.log(`${Object.keys(cells).length} cells re-judged, ${failing} miss #239's definition of done`);
  process.exit(check && failing ? 1 : 0);
}

const results = {};
const failures = [];
async function record(page, key, scenario, count, { settled = true } = {}) {
  if (settled) await settle(page);
  const name = `${key}--p${count}--${scenario}`;
  await page.screenshot({ path: join(outDir, `${name}.png`) });
  const metrics = await measure(page);
  const issues = problems(key, scenario, count, metrics);
  const ceiling = twoPersonCeiling(key, count, scenario, metrics);
  results[name] = { device: DEVICES[key].label, scenario, participants: count, ...metrics, issues };
  if (issues.length) failures.push(`${name}: ${issues.join(' | ')}`);
  console.log(
    `${issues.length ? 'FAIL' : 'ok  '} ${name.padEnd(36)} tiles=${metrics.tileAreaFraction.toFixed(2)} video=${metrics.videoAreaFraction.toFixed(2)} ` +
      `visible=${metrics.visibleNow}/${metrics.tiles} hero=${metrics.hero ?? '-'} self=${metrics.selfThumbnail ?? '-'}` +
      (ceiling !== null ? ` ceiling=${ceiling.toFixed(2)}` : '') +
      (scenario === 'share-spotlight' ? ` heroVideo=${metrics.heroVideoArea} largestThumb=${metrics.largestThumbnailArea}` : '') +
      (metrics.camerasWaiting.length ? ` waiting=${metrics.camerasWaiting.join(',')}` : '') +
      (issues.length ? `  <- ${issues.join(' | ')}` : '')
  );
}

const browser = await chromium.launch({
  headless: true,
  args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream', '--autoplay-policy=no-user-gesture-required', '--no-sandbox'],
});
try {
  const passes = counts.map((count) => ({ count, share: false }));
  if (withShare) passes.push({ count: Math.max(4, ...counts.filter((c) => c <= 4)), share: true });
  for (const { count, share } of passes) {
    const code = roomCode();
    const peers = await spawnPeers(browser, code, count - 1);
    if (share && peers[0]) {
      await peers[0].page.evaluate(() => window.__petalHarness.cockpitAutoScenario.sharePattern());
      await peers[0].page.waitForTimeout(1500);
    }
    if (!share && withChat && count === 4) {
      for (let index = 0; index < 8; index += 1) {
        await peers[index % peers.length].page.evaluate(
          (text) => window.__petalHarness.chat.send(text),
          `Message ${index + 1}: the quick brown fox jumps over the lazy dog`
        );
      }
    }
    for (const key of deviceKeys) {
      const viewer = await openViewer(browser, code, key);
      if (share) {
        await viewer.page
          .waitForFunction(() => document.querySelector('.share-tile video')?.readyState >= 2, null, { timeout: 15_000 })
          .catch(() => {});
        await viewer.page.waitForTimeout(600);
        // The first share auto-spotlights itself: that is the hero cell.
        await record(viewer.page, key, 'share-spotlight', count);
        await setLayout(viewer.page, 'grid');
        await record(viewer.page, key, 'share-grid', count);
      } else {
        await setLayout(viewer.page, 'grid');
        await record(viewer.page, key, 'grid', count);
        if (count > 1) {
          await setLayout(viewer.page, 'spotlight');
          await record(viewer.page, key, 'spotlight', count);
        }
        if (withChat && count === 4) {
          await setLayout(viewer.page, 'grid');
          await viewer.page.evaluate(() => document.querySelector('#ctl-chat')?.click());
          await viewer.page.waitForTimeout(800);
          await record(viewer.page, key, 'chat', count);
          if (PORTRAIT_PHONES.includes(key)) {
            // The keyboard up (interactive-widget=resizes-content shrinks the
            // layout viewport): wider than tall, but a portrait phone's bar.
            const viewport = viewer.page.viewportSize();
            await viewer.page.evaluate(() => document.querySelector('.chat-input')?.focus());
            await viewer.page.setViewportSize({ width: viewport.width, height: Math.round(viewport.width * 0.82) });
            await viewer.page.waitForTimeout(800);
            await record(viewer.page, key, 'keyboard', count, { settled: false });
            await viewer.page.setViewportSize(viewport);
            await viewer.page.waitForTimeout(400);
          }
          await viewer.page.evaluate(() => document.querySelector('#ctl-chat')?.click());
        }
        if (count === 4 && (await railLayout(viewer.page))) {
          // The faded landscape top bar, brought back by a tap on a tile.
          await settle(viewer.page);
          const tile = await viewer.page.locator('#tiles .tile').first().boundingBox();
          if (tile) await viewer.page.touchscreen.tap(tile.x + tile.width / 2, tile.y + tile.height / 2);
          await viewer.page.waitForTimeout(400);
          await record(viewer.page, key, 'topbar', count, { settled: false });
        }
      }
      await leave(viewer);
    }
    for (const peer of peers) await leave(peer);
  }
} finally {
  writeFileSync(join(outDir, 'metrics.json'), JSON.stringify(results, null, 2));
  await browser.close();
}

console.log(`\n${Object.keys(results).length} cells, screenshots + metrics.json in ${outDir}`);
if (failures.length) {
  console.log(`${failures.length} cell(s) miss #239's definition of done:`);
  for (const failure of failures) console.log(`  ${failure}`);
}
process.exit(check && failures.length ? 1 : 0);
