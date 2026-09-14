// Live smoke driver (not a test): three headless web peers on the local dev
// server (`VITE_PETAL_BACKEND_URL=https://app.petal.live npx vite --port 5173`)
// exercise meeting chat over the real backend and SFU: A sends, B sees the
// badge, opens the drawer, reads and replies; C joins late and receives the
// history. Prints PASS/FAIL per step and exits non-zero on the first failure.
//   node --import tsx web-harness/tests/fixtures/chat/live-chat-smoke.ts [access-code]
// With an access code, peers join an existing meeting (e.g. one the desktop
// app created) instead of a fresh one, and the late-joiner step is skipped
// unless there are already messages to relay.
import { createRequire } from 'node:module';
import { resolve } from 'node:path';
import { generateAccessCode } from '@petal/shared/logic/meetingCode';

const repoRoot = resolve(import.meta.dirname, '../../../..');
const { chromium } = createRequire(import.meta.url)(resolve(repoRoot, 'apps/desktop/node_modules/playwright')) as typeof import('playwright');

const base = process.env.PETAL_WEB_URL ?? 'http://localhost:5173';
const code = process.argv[2] ?? generateAccessCode();
const T = 60_000;

type Msg = { id: string; text: string; sender: { identity: string; name: string | null }; self: boolean; relayed: boolean };
declare global {
  interface Window {
    __petalHarness: {
      room: { state: string; localParticipant: { identity: string }; remoteParticipants: Map<string, { identity: string; name?: string }> } | null;
      chat: { open: boolean; setOpen(o: boolean): void; messages(): readonly Msg[]; send(t: string): Promise<void> } | null;
    };
  }
}

let failed = false;
function step(name: string, ok: boolean, detail = ''): void {
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? `  (${detail})` : ''}`);
  if (!ok) failed = true;
}

const browser = await chromium.launch({
  headless: true,
  args: ['--no-sandbox', '--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream'],
});

async function peer(name: string) {
  const context = await browser.newContext({ viewport: { width: 1000, height: 720 } });
  const page = await context.newPage();
  page.on('console', (m) => {
    const t = m.text();
    if (/chat|error|unhandled/i.test(t) && !/favicon/.test(t)) console.log(`[${name}] ${m.type()} ${t.slice(0, 160)}`);
  });
  await page.addInitScript((n: string) => {
    try {
      localStorage.setItem('petal-harness-name', n);
    } catch {}
  }, name);
  await page.goto(`${base}/${code}`);
  await page.waitForFunction(() => window.__petalHarness?.room?.state === 'connected', null, { timeout: T });
  const identity = await page.evaluate(() => window.__petalHarness.room!.localParticipant.identity);
  console.log(`[${name}] connected as ${identity} in ${code}`);
  return { name, page, identity };
}

async function waitMessages(p: Awaited<ReturnType<typeof peer>>, count: number): Promise<readonly Msg[]> {
  await p.page.waitForFunction((n: number) => (window.__petalHarness.chat?.messages().length ?? 0) >= n, count, { timeout: T });
  return p.page.evaluate(() => window.__petalHarness.chat!.messages().map((m) => ({ ...m })));
}

try {
  const a = await peer('Peer A');
  const b = await peer('Peer B');
  // Both see each other before anyone sends (reliable data only reaches subscribed peers).
  for (const [x, y] of [
    [a, b],
    [b, a],
  ] as const) {
    await x.page.waitForFunction((id: string) => [...(window.__petalHarness.room?.remoteParticipants.values() ?? [])].some((r) => r.identity === id), y.identity, { timeout: T });
  }
  step('A and B are connected and see each other', true);

  const preexisting = (await a.page.evaluate(() => window.__petalHarness.chat!.messages().length)) as number;

  // A sends while B's drawer is closed.
  await a.page.evaluate(() => window.__petalHarness.chat!.send('hello from A'));
  const bGot = await waitMessages(b, preexisting + 1);
  const first = bGot.at(-1)!;
  step('B receives A\'s message', first.text === 'hello from A' && !first.self && !first.relayed, `${first.sender.name ?? '?'}: ${first.text}`);
  step('B attributes it to A by the authenticated identity', first.sender.identity === a.identity && first.sender.name === 'Peer A', `${first.sender.identity} / ${first.sender.name}`);
  const badge = await b.page.evaluate(() => {
    const el = document.getElementById('ctl-chat-badge')!;
    return { hidden: el.hidden, text: el.textContent, label: document.getElementById('ctl-chat')!.getAttribute('aria-label') };
  });
  step('B\'s Chat control shows 1 unread', !badge.hidden && badge.text === '1' && badge.label === 'Open chat, 1 unread', JSON.stringify(badge));
  const toast = await b.page.evaluate(() => document.body.innerText.includes('Peer A: hello from A'));
  step('B saw the closed-drawer notice', toast);

  // B opens the drawer: the message is there, the badge clears, and B replies from the composer.
  await b.page.click('#ctl-chat');
  await b.page.locator('[data-testid="chat-msg"]').first().waitFor({ timeout: 10_000 });
  const drawer = await b.page.evaluate(() => ({
    texts: [...document.querySelectorAll('[data-testid="chat-msg"] .chat-text')].map((e) => e.textContent),
    names: [...document.querySelectorAll('[data-testid="chat-msg"] .chat-name')].map((e) => e.textContent),
    badgeHidden: document.getElementById('ctl-chat-badge')!.hidden,
    asideVisible: !document.getElementById('chat-drawer')!.hidden,
  }));
  step('B\'s drawer lists the message under A\'s name and clears the badge', drawer.texts.at(-1) === 'hello from A' && drawer.names.at(-1) === 'Peer A' && drawer.badgeHidden && drawer.asideVisible, JSON.stringify(drawer));
  await b.page.locator('[data-testid="chat-input"]').click();
  await b.page.keyboard.type('hi from B');
  await b.page.keyboard.press('Enter');
  const aGot = await waitMessages(a, preexisting + 2);
  const reply = aGot.at(-1)!;
  step('A receives B\'s reply sent from the composer', reply.text === 'hi from B' && reply.sender.name === 'Peer B' && !reply.self, `${reply.sender.name}: ${reply.text}`);
  step('A\'s own message is marked self', aGot.some((m) => m.text === 'hello from A' && m.self));
  await b.page.screenshot({ path: '/tmp/petal-chat-b-drawer.png' });

  // C joins late and gets the history from the peers.
  const c = await peer('Peer C');
  const cGot = await waitMessages(c, preexisting + 2);
  const texts = cGot.map((m) => m.text);
  step('late joiner C receives the history in order', texts.slice(-2).join(' | ') === 'hello from A | hi from B', texts.join(' | '));
  const relayed = cGot.slice(-2).map((m) => `${m.sender.name}:${m.relayed}`);
  step('history entries carry their original senders', cGot.slice(-2).every((m) => m.sender.name === 'Peer A' || m.sender.name === 'Peer B'), relayed.join(', '));
  const cUnread = await c.page.evaluate(() => document.getElementById('ctl-chat-badge')!.hidden);
  step('history does not count as unread for C', cUnread);
  await c.page.click('#ctl-chat');
  await c.page.locator('[data-testid="chat-msg"]').first().waitFor({ timeout: 10_000 });
  await c.page.screenshot({ path: '/tmp/petal-chat-c-history.png' });
  console.log(failed ? 'RESULT: FAIL' : 'RESULT: PASS', `(meeting ${code}; screenshots in /tmp/petal-chat-*.png)`);
} catch (e) {
  failed = true;
  console.log('RESULT: FAIL', e instanceof Error ? e.message : String(e));
} finally {
  await browser.close();
}
process.exit(failed ? 1 : 0);
