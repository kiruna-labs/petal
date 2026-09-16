// Live smoke driver (not a test) for the native<->web chat gate: one headless
// web peer joins a meeting the DESKTOP dev app is already in, and records the
// sequence the reviewer asked for:
//   1. native sends           -> web badge shows 1 unread and the closed-drawer notice appears
//   2. web replies            -> (native receives; the person at the desktop confirms)
//   3. native leaves, rejoins -> native's history-req is observed and answered by this peer
// Prints PASS/FAIL per step plus a log of every petal.chat packet from the
// native identity, and saves /tmp/petal-chat-native-web.png. Exits non-zero
// on a failed step. Prompts for each human action on stdout.
//   node --import tsx tests/fixtures/chat/live-native-chat-smoke.ts <access-code> [secs-per-step=180]
// Needs `VITE_PETAL_BACKEND_URL=https://app.petal.live npx vite --port 5173`.
import { createRequire } from 'node:module';
import { resolve } from 'node:path';

const repoRoot = resolve(import.meta.dirname, '../../../..');
// Untyped on purpose: playwright is the desktop package's dependency, not this one's.
const { chromium } = createRequire(import.meta.url)(resolve(repoRoot, 'apps/desktop/node_modules/playwright'));

const [code, secsArg = '180'] = process.argv.slice(2);
if (!code) {
  console.error('usage: live-native-chat-smoke.ts <access-code> [secs-per-step]');
  process.exit(2);
}
const base = process.env.PETAL_WEB_URL ?? 'http://localhost:5173';
const T = Number(secsArg) * 1000;

type Msg = ChatSmokeMsg;
type Packet = ChatSmokePacket;

let failed = false;
function step(name: string, ok: boolean, detail = ''): void {
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? `  (${detail})` : ''}`);
  if (!ok) failed = true;
}
function prompt(text: string): void {
  console.log(`\n>>> ${text}\n`);
}
const isNative = (identity: string) => !identity.startsWith('web-');

const browser = await chromium.launch({ headless: true, args: ['--no-sandbox', '--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream'] });
const page = await browser.newPage({ viewport: { width: 1000, height: 720 } });
await page.addInitScript(() => {
  try {
    localStorage.setItem('petal-harness-name', 'Web Peer (Claude)');
  } catch {}
});
await page.goto(`${base}/${code}`);
await page.waitForFunction(() => window.__petalHarness?.room?.state === 'connected', null, { timeout: 60_000 });
// Record every chat packet from a native identity, independent of the store.
await page.evaluate(() => {
  window.__chatPackets = [];
  window.__petalHarness.room!.on('dataReceived', (payload: unknown, participant: unknown, _kind: unknown, topic: unknown) => {
    if (topic !== 'petal.chat') return;
    const p = participant as { identity: string; name?: string } | undefined;
    // A packet from a participant the client has not resolved yet still carries
    // its identity through the SFU; log it as unresolved rather than dropping it.
    const identity = p?.identity ?? 'unresolved';
    if (identity.startsWith('web-')) return;
    try {
      const wire = JSON.parse(new TextDecoder().decode(payload as Uint8Array));
      window.__chatPackets.push({ at: new Date().toISOString().slice(11, 19), from: identity, name: p?.name ?? '', type: wire.type, text: wire.text, count: wire.messages?.length });
    } catch {}
  });
});
const me = (await page.evaluate(() => window.__petalHarness.room!.localParticipant.identity)) as string;
const remotes = (await page.evaluate(() => [...window.__petalHarness.room!.remoteParticipants.values()].map((p) => `${p.identity} (${p.name ?? 'no name'})`))) as string[];
console.log(`[web] connected as ${me} in ${code}; remotes: ${remotes.join(', ') || 'none'}`);
if (!remotes.some((r) => isNative(r.split(' ')[0]))) prompt(`No native participant yet: join ${code} from the desktop dev app (waiting up to ${secsArg}s).`);
await page.waitForFunction(() => [...window.__petalHarness.room!.remoteParticipants.values()].some((p) => !p.identity.startsWith('web-')), null, { timeout: T });
const native = (await page.evaluate(() => [...window.__petalHarness.room!.remoteParticipants.values()].find((p) => !p.identity.startsWith('web-'))!.identity)) as string;
step('a native participant is in the meeting', true, native);
// Only messages that arrive LIVE from a native identity after this point count
// (history relayed on join is marked relayed and excluded).
const liveNativeCount = () => window.__petalHarness.chat!.messages().filter((m) => !m.self && !m.relayed && !m.sender.identity.startsWith('web-')).length;
const before = (await page.evaluate(liveNativeCount)) as number;

// 1. native sends while our drawer is closed.
prompt(`Step 1: on the DESKTOP, open Chat and send any message now (waiting up to ${secsArg}s).`);
await page.waitForFunction((n: number) => window.__petalHarness.chat!.messages().filter((m) => !m.self && !m.relayed && !m.sender.identity.startsWith('web-')).length > n, before, { timeout: T });
const live = (await page.evaluate(() => window.__petalHarness.chat!.messages().filter((m) => !m.self && !m.relayed && !m.sender.identity.startsWith('web-')).at(-1))) as Msg;
step('web receives the native message live', !!live, `${live.sender.name}: ${live.text}`);
const badge = await page.evaluate(() => {
  const el = document.getElementById('ctl-chat-badge')!;
  return { hidden: el.hidden, text: el.textContent, label: document.getElementById('ctl-chat')!.getAttribute('aria-label') };
});
step('web Chat control shows unread', !badge.hidden && /^\d+$/.test(badge.text ?? '') && (badge.label ?? '').startsWith('Open chat, '), JSON.stringify({ badge: badge.text, label: badge.label }));
const notice = (await page.evaluate((t: string) => document.body.innerText.includes(t), `${live.sender.name}: ${live.text}`.slice(0, 40))) as boolean;
step('web shows the closed-drawer notice', notice);

// 2. web opens the drawer and replies from the composer.
await page.click('#ctl-chat');
await page.locator('[data-testid="chat-msg"]').first().waitFor({ timeout: 10_000 });
const listed = await page.evaluate(() => ({
  texts: [...document.querySelectorAll('[data-testid="chat-msg"] .chat-text')].map((e) => e.textContent),
  badgeHidden: document.getElementById('ctl-chat-badge')!.hidden,
}));
step('web drawer lists the native message and clears the badge', listed.texts.includes(live.text) && listed.badgeHidden, JSON.stringify(listed));
const replyText = `Reply from the web peer at ${new Date().toISOString().slice(11, 19)}`;
await page.locator('[data-testid="chat-input"]').click();
await page.keyboard.type(replyText);
await page.keyboard.press('Enter');
step('web sent a reply from the composer', (await page.evaluate((t: string) => window.__petalHarness.chat!.messages().some((m) => m.self && m.text === t), replyText)) as boolean, replyText);
await page.screenshot({ path: '/tmp/petal-chat-native-web.png' });
prompt(`Step 2: confirm on the DESKTOP that "${replyText}" appeared in its drawer.`);

// 3. native leaves and rejoins: its history request must arrive and be answered.
const nativeId = live.sender.identity;
prompt(`Step 3: on the DESKTOP, leave the meeting and rejoin ${code} (waiting up to ${secsArg}s for the rejoin).`);
await page.waitForFunction((id: string) => ![...window.__petalHarness.room!.remoteParticipants.values()].some((p) => p.identity === id), nativeId, { timeout: T });
console.log('[web] native participant left');
const packetsBeforeRejoin = (await page.evaluate(() => window.__chatPackets.length)) as number;
await page.waitForFunction(() => [...window.__petalHarness.room!.remoteParticipants.values()].some((p) => !p.identity.startsWith('web-')), null, { timeout: T });
console.log('[web] a native participant is back');
await page.waitForFunction((n: number) => window.__chatPackets.slice(n).some((p) => p.type === 'history-req'), packetsBeforeRejoin, { timeout: 30_000 }).catch(() => {});
const packets = (await page.evaluate(() => window.__chatPackets)) as Packet[];
const req = packets.slice(packetsBeforeRejoin).filter((p) => p.type === 'history-req');
const ours = (await page.evaluate(() => window.__petalHarness.chat!.messages().length)) as number;
step('native asked for history after rejoining', req.length > 0, `${req.length} request(s); this peer holds ${ours} message(s) to relay`);
console.log('[web] packets from native:', packets.map((p) => `${p.at} ${p.type}${p.text ? ` "${p.text}"` : ''}${p.count !== undefined ? ` x${p.count}` : ''}`).join(' | '));
prompt('Step 3b: confirm on the DESKTOP that the drawer shows the earlier messages in order (its own first, then the web reply).');
console.log(failed ? 'RESULT: FAIL' : 'RESULT: PASS (web-side steps; desktop-side confirmations are the human statements above)', `meeting ${code}; screenshot /tmp/petal-chat-native-web.png`);
await browser.close();
process.exit(failed ? 1 : 0);
