// Live smoke driver (not a test): three headless web peers on the local dev
// server (`VITE_PETAL_BACKEND_URL=https://app.petal.live npx vite --port 5173`)
// exercise meeting chat over the real backend and SFU: A sends, B sees the
// badge, opens the drawer, reads and replies; A runs `/timer` from the
// composer and B sees the Timer plugin's posts labelled "via Timer" while A's
// private usage answer stays with A; C joins late and receives both the typed
// history and the plugin posts. Prints PASS/FAIL per step and exits non-zero
// on the first failure.
//   node --import tsx web-harness/tests/fixtures/chat/live-chat-smoke.ts [access-code]
// With an access code, peers join an existing meeting (e.g. one the desktop
// app created) instead of a fresh one, and the late-joiner step is skipped
// unless there are already messages to relay.
import { createRequire } from 'node:module';
import { resolve } from 'node:path';
import { generateAccessCode } from '@petal/shared/logic/meetingCode';

const repoRoot = resolve(import.meta.dirname, '../../../..');
// Untyped on purpose: playwright is the desktop package's dependency, not this one's (same as pluginSandboxRendered.test.ts).
const { chromium } = createRequire(import.meta.url)(resolve(repoRoot, 'apps/desktop/node_modules/playwright'));

const base = process.env.PETAL_WEB_URL ?? 'http://localhost:5173';
const code = process.argv[2] ?? generateAccessCode();
const T = 60_000;

type Msg = ChatSmokeMsg;

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
  page.on('console', (m: { type(): string; text(): string }) => {
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
  // A runs /timer from the real composer (autocomplete, Enter). The Timer
  // built-in answers privately when the command is empty, and posts for
  // everyone when it starts and ends.
  await a.page.click('#ctl-chat');
  const aInput = a.page.locator('[data-testid="chat-input"]');
  await aInput.click();
  await a.page.keyboard.type('/ti');
  const suggestion = await a.page.locator('[data-testid="chat-suggestion"]').first().textContent({ timeout: 10_000 });
  step('A\'s composer offers /timer from the Timer built-in', /\/timer/.test(suggestion ?? '') && /Timer$/.test((suggestion ?? '').trim()), (suggestion ?? '').replace(/\s+/g, ' ').trim());
  await a.page.keyboard.press('Tab');
  await a.page.keyboard.press('Enter');
  await a.page.waitForFunction(() => window.__petalHarness.chat!.messages().some((m) => m.local), null, { timeout: T });
  const privateAnswer = (await a.page.evaluate(() => window.__petalHarness.chat!.messages().find((m) => m.local))) as Msg;
  step('an empty /timer gets a private usage answer from Timer', privateAnswer.via?.name === 'Timer' && /^Usage: \/timer/.test(privateAnswer.text), privateAnswer.text);
  await aInput.fill('/timer 3s smoke');
  await a.page.keyboard.press('Enter');
  const started = '⏱ Timer started: smoke, 3 s';
  const ended = "⏱ Time's up: smoke (3 s)";
  await b.page.waitForFunction((t: string) => window.__petalHarness.chat!.messages().some((m) => m.text === t), started, { timeout: T });
  const bStart = (await b.page.evaluate((t: string) => window.__petalHarness.chat!.messages().find((m) => m.text === t), started)) as Msg;
  step('B sees the Timer post as Peer A via Timer', bStart.sender.name === 'Peer A' && bStart.via?.id === 'petal.timer' && bStart.via?.name === 'Timer' && !bStart.local, `${bStart.sender.name} via ${bStart.via?.name}: ${bStart.text}`);
  await b.page.waitForFunction((t: string) => window.__petalHarness.chat!.messages().some((m) => m.text === t), ended, { timeout: T });
  step("B sees Time's up after the timer ends", true, ended);
  const bLeak = (await b.page.evaluate(() => window.__petalHarness.chat!.messages().some((m) => m.local || /^Usage:/.test(m.text)))) as boolean;
  step('the private answer never reached B', !bLeak);
  const bVia = await b.page.evaluate(() =>
    [...document.querySelectorAll('[data-testid="chat-via"]')].map((el) => el.textContent?.replace(/\s+/g, ' ').trim()),
  );
  step('B\'s drawer draws the via line', bVia.includes('via Timer'), JSON.stringify(bVia));
  await b.page.screenshot({ path: '/tmp/petal-chat-b-drawer.png' });
  await a.page.screenshot({ path: '/tmp/petal-chat-a-timer.png' });

  // C joins late and gets the history from the peers: typed messages and plugin posts.
  const c = await peer('Peer C');
  const cGot = await waitMessages(c, preexisting + 4);
  const typed = cGot.filter((m) => !m.via);
  const texts = typed.map((m) => m.text);
  step('late joiner C receives the history in order', texts.slice(-2).join(' | ') === 'hello from A | hi from B', texts.join(' | '));
  const relayed = typed.slice(-2).map((m) => `${m.sender.name}:${m.relayed}`);
  step('history entries carry their original senders', typed.slice(-2).every((m) => m.sender.name === 'Peer A' || m.sender.name === 'Peer B'), relayed.join(', '));
  const cPosts = cGot.filter((m) => m.via).map((m) => `${m.sender.name} via ${m.via!.name}: ${m.text}`);
  step('C receives the Timer posts with their via stamp, and no private answer', cPosts.length === 2 && cPosts.every((p) => p.startsWith('Peer A via Timer: ⏱')) && !cGot.some((m) => m.local), cPosts.join(' | '));
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
