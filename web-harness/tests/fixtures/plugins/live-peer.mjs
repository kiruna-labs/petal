// Live smoke driver (not a test): joins a meeting as a web peer on the local
// dev server and either sends a reaction or waits for one from the native
// side. Usage:
//   node live-peer.mjs <access-code> send        # click React -> 👍, exit
//   node live-peer.mjs <access-code> wait [secs] # wait for a remote reaction in the overlay
import { createRequire } from 'node:module';
import { resolve } from 'node:path';
const repoRoot = resolve(import.meta.dirname, '../../../..');
const { chromium } = createRequire(import.meta.url)(resolve(repoRoot, 'apps/desktop/node_modules/playwright'));

const [code, mode = 'send', secsArg = '90'] = process.argv.slice(2);
if (!code) { console.error('usage: live-peer.mjs <access-code> send|wait [secs]'); process.exit(2); }
const browser = await chromium.launch({ headless: true, args: ['--no-sandbox', '--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream'] });
const page = await browser.newPage({ viewport: { width: 900, height: 700 } });
page.on('console', (m) => { const t = m.text(); if (/plugin|error|reaction|unhandled/i.test(t)) console.log('[web]', m.type(), t.slice(0, 200)); });
await page.addInitScript(() => { try { localStorage.setItem('petal-harness-name', 'Web Peer (Claude)'); } catch {} });
await page.goto(`http://localhost:5173/${code}`);
await page.waitForFunction(() => document.querySelector('#meeting-screen') && !document.querySelector('#meeting-screen').classList.contains('hidden'), null, { timeout: 60_000 });
console.log('[web] meeting screen shown');
await page.waitForFunction(() => (window.__petalHarness && window.__petalHarness.room && window.__petalHarness.room.state === 'connected'), null, { timeout: 60_000 });
const info = await page.evaluate(() => ({ roomName: window.__petalHarness.room.name, identity: window.__petalHarness.room.localParticipant.identity, remotes: [...window.__petalHarness.room.remoteParticipants.values()].map(p => p.identity + ':' + (p.name||'')), plugins: !!window.__petalHarness.plugins, loaded: window.__petalHarness.plugins?.host.loaded().map((p) => p.manifest.id) }));
console.log('[web] connected as', info.identity, 'room:', info.roomName, 'remotes:', info.remotes, 'pluginsHook:', info.plugins);
await page.screenshot({ path: '/tmp/petal-web-peer-joined.png' });
const reactCell = page.locator('.plugin-control-cell[data-plugin="petal.reactions"] button');
await reactCell.waitFor({ timeout: 15_000 });
console.log('[web] React control present');
const overlay = page.frameLocator('iframe.petal-plugin-surface-overlay');

if (mode === 'debug') {
  await page.waitForTimeout(2500);
  const dump = await page.evaluate(async () => {
    const room = window.__petalHarness.room;
    const lines = [...document.querySelectorAll('#session-log *')].map((el) => el.textContent || '').filter((t) => /plugin|advert|metadata|unhandled/i.test(t));
    return { lines };
  }).catch(async () => ({ lines: await page.evaluate(() => [...document.querySelectorAll('#session-log *')].map((el) => el.textContent || '').filter((t) => /plugin|advert|metadata|unhandled/i.test(t))) }));
  console.log('[web] session log (plugin-related):'); for (const l of dump.lines.slice(-15)) console.log('   ', l.slice(0, 220));
  const probe = await page.evaluate(async () => {
    const room = window.__petalHarness.room;
    const lp = room.localParticipant;
    const before = lp.metadata;
    let setErr = null;
    try { await lp.setMetadata(JSON.stringify({ ...(JSON.parse(before || '{}')), probe: 1 })); } catch (e) { setErr = String(e && e.message || e); }
    await new Promise((r) => setTimeout(r, 2500));
    const grants = (() => { try { const t = room.engine?.token || room.engine?.client?.token; return t ? JSON.parse(atob(t.split('.')[1])).video : null; } catch { return null; } })();
    return { before, after: lp.metadata, setErr, grants, state: room.state, loaded: window.__petalHarness.plugins?.host.loaded().map((p) => p.manifest.id), hasSetMetadata: typeof lp.setMetadata };
  });
  console.log('[web] metadata probe:', JSON.stringify(probe, null, 1));
  window_readvertise: {
    await page.evaluate(() => window.__petalHarness.plugins.host.readvertise());
    await page.waitForTimeout(2500);
    console.log('[web] after readvertise():', await page.evaluate(() => window.__petalHarness.room.localParticipant.metadata));
    console.log('[web] session log tail:', await page.evaluate(() => [...document.querySelectorAll('#session-log *')].map((el) => el.textContent || '').filter((t) => /plugin|advert|metadata/i.test(t)).slice(-6)));
  }
} else if (mode === 'send') {
  await reactCell.click();
  const popover = page.frameLocator('iframe.petal-plugin-surface-popover');
  await popover.locator('button[aria-label="React with 👍"]').click({ timeout: 15_000 });
  await overlay.locator('.r .e').first().waitFor({ timeout: 10_000 });
  const t0 = Date.now();
  let landed = null;
  while (Date.now() - t0 < 20000 && !landed) {
    landed = await page.evaluate(() => JSON.parse(window.__petalHarness.room.localParticipant.metadata || '{}').plugins ?? null);
    if (!landed) await page.waitForTimeout(250);
  }
  console.log('[web] sent 👍 (local echo visible); metadata plugins key:', landed, landed ? `(landed after ${Date.now() - t0} ms)` : '(never landed in 20 s)');
  console.log('[web] session log (plugin lines):', await page.evaluate(() => [...document.querySelectorAll('#session-log *')].map((el) => el.textContent || '').filter((t) => /plugin|advert|metadata/i.test(t) && /PM|AM/.test(t)).slice(-6)));
  await page.waitForTimeout(500);
  await page.screenshot({ path: '/tmp/petal-web-peer-sent.png' });
} else {
  console.log(`[web] waiting up to ${secsArg}s for a reaction from another participant...`);
  const deadline = Date.now() + Number(secsArg) * 1000;
  let seen = null;
  while (Date.now() < deadline && !seen) {
    const items = await overlay.locator('.r').evaluateAll((els) => els.map((el) => el.textContent)).catch(() => []);
    if (items.length) seen = items;
    else await page.waitForTimeout(500);
  }
  if (seen) { console.log('[web] RECEIVED reaction(s):', seen); await page.screenshot({ path: '/tmp/petal-web-peer-received.png' }); }
  else console.log('[web] no reaction received before the deadline');
  console.log('[web] remote metadata plugins:', await page.evaluate(() => [...window.__petalHarness.room.remoteParticipants.values()].map(p => [p.identity, JSON.parse(p.metadata||'{}').plugins])));
}
await browser.close();
