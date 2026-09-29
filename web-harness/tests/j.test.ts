// Moved from backend/test/distribution.ts alongside api/j.ts itself (see
// api/j.ts's header comment) -- join links now live at meet.petal.live.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import type { VercelRequest, VercelResponse } from '../api/_lib/vercel.js';
import joinHandler, {
  desktopDownloadPlatformForUserAgent,
  downloadUrlForPlatform,
  isPhoneOrTabletUserAgent,
  webJoinUrlForAccessCode,
} from '../api/j.ts';
import { HARNESS_REJOIN_SESSION_KEY } from '../src/constants.ts';

const contractFixture = JSON.parse(
  readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8'),
) as {
  inviteLinks: Array<{
    label: string;
    accessCode: string;
    credential: string;
    httpsPath: string;
    nativeDeepLink: string;
    webJoinQuery: string;
  }>;
};

type TestResponse = {
  statusCode: number;
  headers: Record<string, string | number | readonly string[]>;
  body: unknown;
  ended: boolean;
  status(code: number): TestResponse;
  json(body: unknown): TestResponse;
  send(body: unknown): TestResponse;
  end(body?: unknown): TestResponse;
  setHeader(name: string, value: string | number | readonly string[]): TestResponse;
};

function req(
  method: string,
  query?: Record<string, string>,
  headers: Record<string, string> = {},
  url?: string,
): VercelRequest {
  return { method, query: query ?? {}, headers, url } as unknown as VercelRequest;
}

function res(): TestResponse {
  return {
    statusCode: 200,
    headers: {},
    body: undefined,
    ended: false,
    status(code: number) {
      this.statusCode = code;
      return this;
    },
    json(body: unknown) {
      this.body = body;
      this.ended = true;
      return this;
    },
    send(body: unknown) {
      this.body = body;
      this.ended = true;
      return this;
    },
    end(body?: unknown) {
      this.body = body;
      this.ended = true;
      return this;
    },
    setHeader(name: string, value: string | number | readonly string[]) {
      this.headers[name.toLowerCase()] = value;
      return this;
    },
  };
}

async function call(
  method: string,
  query?: Record<string, string>,
  headers: Record<string, string> = {},
  url?: string,
): Promise<TestResponse> {
  const response = res();
  await joinHandler(req(method, query, headers, url), response as unknown as VercelResponse);
  return response;
}

test('/<label>/<access-code> returns native-launch interstitial with fallbacks', async () => {
  const [vector] = contractFixture.inviteLinks;
  const response = await call('GET', { label: vector.label, code: vector.accessCode.toUpperCase() });

  assert.equal(response.statusCode, 200);
  assert.equal(response.headers['content-type'], 'text/html; charset=utf-8');
  assert.equal(typeof response.body, 'string');
  const body = response.body as string;
  assert.match(body, /Opening the desktop app/);
  assert.match(body, /href="https:\/\/app\.petal\.live\/api\/download\?platform=macos"/);
  assert.match(body, /Join in browser/);
  assert.match(body, new RegExp(vector.nativeDeepLink.replace(/\//g, '\\/')));
  assert.match(body, /window\.location\.href = "petal:\/\/join\/abc-defg-hjk"/);
  assert.match(body, new RegExp(`https:\\/\\/meet\\.petal\\.live\\/${vector.webJoinQuery.replace('?', '\\?')}`));
  assert.ok(!body.includes(vector.credential), 'hidden credential is not rendered');
  assert.match(body, /class="brand-mark" width="32" height="32"/);
  assert.match(body, /\.brand-mark \{[^}]*color: #f5f6f7;/, 'the mark is drawn at full opacity');
  assert.match(body, />Download Petal for macOS</);
  assert.match(body, /href="https:\/\/app\.petal\.live\/api\/download\?platform=windows"/);
  assert.match(body, />Download Petal for Windows</);
  assert.match(body, /id="code-copy"/);
  assert.match(body, /aria-label="Copy invite link"/);
});

test('desktop download platform detection prefers Windows only for Windows user agents', () => {
  assert.equal(desktopDownloadPlatformForUserAgent('Mozilla/5.0 (Windows NT 10.0; Win64; x64)'), 'windows');
  assert.equal(desktopDownloadPlatformForUserAgent(['Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)']), 'macos');
  assert.equal(downloadUrlForPlatform('windows'), 'https://app.petal.live/api/download?platform=windows');
  assert.equal(downloadUrlForPlatform('macos'), 'https://app.petal.live/api/download?platform=macos');
});

test('Windows invite visitors get Windows as primary and macOS as explicit fallback', async () => {
  const response = await call(
    'GET',
    { label: 'release-test-room', code: 'abc-defg-hjk' },
    { 'user-agent': 'Mozilla/5.0 (Windows NT 10.0; Win64; x64)' },
  );

  assert.equal(response.statusCode, 200);
  const body = response.body as string;
  assert.match(body, /href="https:\/\/app\.petal\.live\/api\/download\?platform=windows"/);
  assert.match(body, /href="https:\/\/app\.petal\.live\/api\/download\?platform=macos"/);
  assert.match(body, /Windows downloads are currently unsigned/);
});

const ANDROID_PHONE_UA = 'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Mobile Safari/537.36';
const ANDROID_TABLET_UA = 'Mozilla/5.0 (Linux; Android 14; SM-X710) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36';
const IPHONE_UA = 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1';
const IPAD_UA = 'Mozilla/5.0 (iPad; CPU OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1';
// Also what iPadOS Safari sends by default ("Request Desktop Website").
const MAC_SAFARI_UA = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15';
const WINDOWS_UA = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36';
const LINUX_UA = 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36';
// Only the `Mobile` token marks this one.
const KAIOS_UA = 'Mozilla/5.0 (Mobile; LYF/F300B/LF-F300B-000-01-15-130718; rv:48.0) Gecko/48.0 Firefox/48.0 KAIOS/2.5';

test('phone and tablet detection matches Android, iPhone, iPad, iPod and Mobile user agents only', () => {
  for (const ua of [ANDROID_PHONE_UA, ANDROID_TABLET_UA, IPHONE_UA, IPAD_UA, KAIOS_UA, 'Mozilla/5.0 (iPod touch; CPU iPhone OS 15_0 like Mac OS X)']) {
    assert.equal(isPhoneOrTabletUserAgent(ua), true, ua);
  }
  for (const ua of [MAC_SAFARI_UA, WINDOWS_UA, LINUX_UA, '']) {
    assert.equal(isPhoneOrTabletUserAgent(ua), false, ua);
  }
  assert.equal(isPhoneOrTabletUserAgent(undefined), false);
});

test('phones and tablets get Join in browser as the only action, no hand-off and no downloads', async () => {
  const visitors: Array<Record<string, string>> = [
    ...[ANDROID_PHONE_UA, ANDROID_TABLET_UA, IPHONE_UA, IPAD_UA, KAIOS_UA].map((ua) => ({ 'user-agent': ua })),
    { 'user-agent': LINUX_UA, 'sec-ch-ua-mobile': '?1' },
  ];
  for (const headers of visitors) {
    const ua = JSON.stringify(headers);
    const response = await call('GET', { label: 'design-review', code: 'abc-defg-hjk' }, headers);

    assert.equal(response.statusCode, 200, ua);
    assert.equal(response.headers['cache-control'], 'private, no-cache', `${ua}: never cached across devices`);
    const body = response.body as string;
    assert.match(body, /<html lang="en" class="mobile">/, ua);
    assert.match(body, /Join design-review/, ua);
    assert.match(
      body,
      /<a class="button primary" href="https:\/\/meet\.petal\.live\/\?code=abc-defg-hjk" rel="noreferrer">Join in browser<\/a>/,
      ua,
    );
    assert.equal(body.match(/class="button/g)?.length, 1, `${ua}: one button`);
    assert.ok(!body.includes('petal://'), `${ua}: no Open Petal link or hand-off script`);
    assert.ok(!body.includes('/api/download'), `${ua}: no download buttons`);
    assert.doesNotMatch(body, /Opening the desktop app/, ua);
    assert.doesNotMatch(body, /navigator\.maxTouchPoints/, `${ua}: no iPadOS check needed`);
    // The desktop apps get one line of their own, under the meeting code.
    assert.match(
      body,
      /id="code-copy"[\s\S]*<p class="mobile-only desktop-apps">On a computer\? <a href="https:\/\/petal\.live\/docs\/getting-started\/install\/" rel="noreferrer">Download Petal for Windows or macOS<\/a><\/p>/,
      ua,
    );
    assert.match(body, /class="brand-mark" width="32" height="32"/, ua);
  }
});

test('desktop visitors keep Open Petal as primary, both downloads and the hand-off', async () => {
  const visitors: Array<Record<string, string>> = [
    ...[MAC_SAFARI_UA, WINDOWS_UA, LINUX_UA].map((ua) => ({ 'user-agent': ua })),
    { 'user-agent': WINDOWS_UA, 'sec-ch-ua-mobile': '?0' },
  ];
  for (const headers of visitors) {
    const ua = JSON.stringify(headers);
    const response = await call('GET', { label: 'design-review', code: 'abc-defg-hjk' }, headers);

    assert.equal(response.statusCode, 200, ua);
    assert.equal(response.headers['cache-control'], 'private, no-cache', `${ua}: never cached across devices`);
    const body = response.body as string;
    assert.match(body, /<html lang="en">/, ua);
    assert.match(body, /<a class="button primary" href="petal:\/\/join\/abc-defg-hjk">Open Petal<\/a>/, ua);
    assert.match(body, /<a class="button secondary" href="https:\/\/meet\.petal\.live\/\?code=abc-defg-hjk" rel="noreferrer">Join in browser<\/a>/, ua);
    assert.match(body, /href="https:\/\/app\.petal\.live\/api\/download\?platform=macos"/, ua);
    assert.match(body, /href="https:\/\/app\.petal\.live\/api\/download\?platform=windows"/, ua);
    assert.match(body, /Opening the desktop app/, ua);
    assert.match(body, /window\.location\.href = "petal:\/\/join\/abc-defg-hjk";\s*\}, 150\);/, ua);
  }
});

// Runs the page's inline scripts in document order against a minimal DOM, then
// any timers they scheduled, and reports what the visitor would end up with.
function runPageScripts(html: string, navigator: { userAgent: string; maxTouchPoints: number }) {
  const classes = new Set<string>();
  const location = { href: 'https://meet.petal.live/design-review/abc-defg-hjk' };
  const timers: Array<() => void> = [];
  const window = {
    location,
    setTimeout(fn: () => void) {
      timers.push(fn);
      return timers.length;
    },
  };
  const document = {
    documentElement: {
      classList: {
        add: (name: string) => classes.add(name),
        contains: (name: string) => classes.has(name),
      },
    },
    getElementById: () => null,
  };
  const context = { window, document, navigator };
  for (const [, source] of html.matchAll(/<script>([\s\S]*?)<\/script>/g)) {
    runInNewContext(source, context);
  }
  for (const fn of timers) fn();
  return { mobile: classes.has('mobile'), href: location.href };
}

test('iPadOS (a Mac user agent with touch) switches to the mobile layout before the hand-off fires', async () => {
  const response = await call('GET', { label: 'design-review', code: 'abc-defg-hjk' }, { 'user-agent': MAC_SAFARI_UA });
  const body = response.body as string;
  // The server cannot tell an iPad from a Mac, so both layouts ship and the
  // page script picks; the mobile one must be the complete phone layout.
  assert.match(body, /<div class="mobile-only">[\s\S]*?<a class="button primary" href="https:\/\/meet\.petal\.live\/\?code=abc-defg-hjk" rel="noreferrer">Join in browser<\/a>/);
  assert.match(body, /<p class="mobile-only desktop-apps">/);
  // What the script's class does: hide the desktop layout, show the mobile one.
  assert.match(body, /\.mobile-only,\s*\.mobile \.desktop-only \{\s*display: none;/);
  assert.match(body, /\.mobile \.mobile-only \{\s*display: block;/);
  const tabletCheck = body.indexOf('navigator.maxTouchPoints');
  assert.ok(
    tabletCheck > -1 && tabletCheck < body.indexOf('</head>'),
    'the iPadOS check runs in <head>, so the desktop layout never paints first',
  );

  const ipad = runPageScripts(body, { userAgent: MAC_SAFARI_UA, maxTouchPoints: 5 });
  assert.equal(ipad.mobile, true, 'iPad shows the mobile layout');
  assert.equal(ipad.href, 'https://meet.petal.live/design-review/abc-defg-hjk', 'iPad is never handed to petal://');

  const mac = runPageScripts(body, { userAgent: MAC_SAFARI_UA, maxTouchPoints: 0 });
  assert.equal(mac.mobile, false, 'a Mac keeps the desktop layout');
  assert.equal(mac.href, 'petal://join/abc-defg-hjk', 'a Mac is handed to the desktop app');
  // Same threshold as the browser client's isPhoneOrTablet (#243).
  assert.equal(runPageScripts(body, { userAgent: MAC_SAFARI_UA, maxTouchPoints: 1 }).mobile, false);

  const windowsBody = (await call('GET', { code: 'abc-defg-hjk' }, { 'user-agent': WINDOWS_UA })).body as string;
  const touchLaptop = runPageScripts(windowsBody, { userAgent: WINDOWS_UA, maxTouchPoints: 10 });
  assert.equal(touchLaptop.mobile, false, 'a Windows touch laptop keeps the desktop layout');
  assert.equal(touchLaptop.href, 'petal://join/abc-defg-hjk');
});

test('/<label>/<access-code> ignores the cosmetic label for authorization', async () => {
  const response = await call('GET', { label: 'not-the-room-name', code: 'abc-defg-hjk' });

  assert.equal(response.statusCode, 200);
  assert.match(response.body as string, /Join not-the-room-name/);
});

test('bare and labeled paths resolve from req.url when no rewrite query is present', async () => {
  const bare = await call('GET', undefined, {}, '/abc-defg-hjk');
  const labeled = await call('GET', undefined, {}, '/renamed-room/abc-defg-hjk');

  assert.equal(bare.statusCode, 200);
  assert.equal(labeled.statusCode, 200);
  assert.match(bare.body as string, /window\.location\.href = "petal:\/\/join\/abc-defg-hjk"/);
  assert.match(labeled.body as string, /Join renamed-room/);
});

test('path and rewritten query must identify the same access code', async () => {
  const response = await call('GET', { code: 'abc-defg-hjk' }, {}, '/other-room/def-ghjk-mnp');

  assert.equal(response.statusCode, 400);
  assert.deepEqual(response.body, { error: 'invalid invite credential' });
});

// Regression: this test previously asserted that i/l codes 400. That pinned a
// real outage in place -- generation excludes i/l, but NORMALIZATION must stay
// broad or every invite issued before the 2026-07-09 narrowing (8a6a456c) dies.
// Canonical behaviour is backend/lib/slug.ts + rooms.rs + shared/logic.
test('legacy i/l access codes still resolve (generation excludes them, parsing must not)', async () => {
  const path = await call('GET', undefined, {}, '/abc-defi-hjk');
  const el = await call('GET', undefined, {}, '/abc-defl-hjk');

  assert.equal(path.statusCode, 200);
  assert.equal(el.statusCode, 200);
  assert.match(path.body as string, /window\.location\.href = "petal:\/\/join\/abc-defi-hjk"/);
});

test('the reported release-test-room invite link resolves', async () => {
  const response = await call('GET', { label: 'release-test-room', code: 'fud-aair-qiz' });

  assert.equal(response.statusCode, 200);
  assert.match(response.body as string, /Join release-test-room/);
  assert.match(response.body as string, /window\.location\.href = "petal:\/\/join\/fud-aair-qiz"/);
});

test('non-letter and wrong-length codes still fail closed', async () => {
  const digits = await call('GET', undefined, {}, '/abc-def0-hjk');
  const short = await call('GET', undefined, {}, '/abc-defg-hj');

  assert.equal(digits.statusCode, 400);
  assert.deepEqual(digits.body, { error: 'invalid invite credential' });
  assert.equal(short.statusCode, 400);
  assert.deepEqual(short.body, { error: 'invalid invite credential' });
});

test('/<label>/<access-code> rejects malformed codes', async () => {
  const response = await call('GET', { label: 'eng-sync', code: 'eng-sync' });

  assert.equal(response.statusCode, 400);
  assert.deepEqual(response.body, { error: 'invalid invite credential' });
});

test('the observed 3-4-4 legacy URL fails closed without minting a credential', async () => {
  const response = await call('GET', { code: 'myq-xfkw-azrp' });
  assert.equal(response.statusCode, 400);
  assert.deepEqual(response.body, { error: 'invalid invite credential' });
});

test('/<label>/<access-code> handles OPTIONS CORS preflight', async () => {
  const response = await call('OPTIONS', undefined, { origin: 'https://meet.petal.live' });

  assert.equal(response.statusCode, 204);
  assert.equal(response.ended, true);
  assert.equal(response.headers['access-control-allow-origin'], 'https://meet.petal.live');
});

test('/<label>/<access-code> rejects non-GET methods', async () => {
  const response = await call('POST', { label: 'eng-sync', code: 'abc-defg-hjk' });

  assert.equal(response.statusCode, 405);
  assert.deepEqual(response.body, { error: 'method not allowed' });
});

test('web join URL can be pointed at the deployed browser client root', async () => {
  const original = process.env.PETAL_WEB_JOIN_URL;
  process.env.PETAL_WEB_JOIN_URL = 'https://petal-web.example/app';
  try {
    assert.equal(webJoinUrlForAccessCode('abc-defg-hjk'), 'https://petal-web.example/?code=abc-defg-hjk');
  } finally {
    if (original === undefined) {
      delete process.env.PETAL_WEB_JOIN_URL;
    } else {
      process.env.PETAL_WEB_JOIN_URL = original;
    }
  }
});

test('web join URL strips stale login path from configured base', async () => {
  const original = process.env.PETAL_WEB_JOIN_URL;
  process.env.PETAL_WEB_JOIN_URL = 'https://web.example/login';
  try {
    assert.equal(webJoinUrlForAccessCode('gax-hagk-jkv'), 'https://web.example/?code=gax-hagk-jkv');
  } finally {
    if (original === undefined) {
      delete process.env.PETAL_WEB_JOIN_URL;
    } else {
      process.env.PETAL_WEB_JOIN_URL = original;
    }
  }
});

// #244: the address bar of a web meeting holds the invite link, so reloading
// the meeting (or a browser restoring the tab) loads this page. Its first
// script sends the tab that was in the meeting back into the app.
type RejoinVisit = {
  navigationType: 'navigate' | 'reload' | 'back_forward';
  wasDiscarded?: boolean;
  /** history.state of the entry being loaded; the meeting's Back guard entry keeps its own. */
  historyState?: Record<string, unknown> | null;
  stored?: string | null;
  storageThrows?: boolean;
};

const GUARD_STATE = { petalMeetingGuard: 'k3x9' };

// Runs the page's inline scripts in document order against a minimal browser,
// the way a parser would: window.stop() ends parsing, so no later script runs.
// Then any timers they set. Reports where the visitor ends up, and what is
// left in this tab's sessionStorage.
function visitInvitePage(html: string, visit: RejoinVisit) {
  const replaced: string[] = [];
  const timers: Array<() => void> = [];
  const listeners = new Map<string, Array<(event: { persisted: boolean }) => void>>();
  const storage = new Map<string, string>();
  if (visit.stored) storage.set(HARNESS_REJOIN_SESSION_KEY, visit.stored);
  let stopped = false;
  const location = {
    href: 'https://meet.petal.live/design-review/abc-defg-hjk',
    replace: (url: string) => replaced.push(url),
  };
  const window = {
    location,
    stop: () => {
      stopped = true;
    },
    setTimeout(fn: () => void) {
      timers.push(fn);
      return timers.length;
    },
    addEventListener(type: string, listener: (event: { persisted: boolean }) => void) {
      listeners.set(type, [...(listeners.get(type) ?? []), listener]);
    },
  };
  const guardStorage = () => {
    if (visit.storageThrows) throw new Error('SecurityError');
  };
  const context = {
    window,
    location,
    history: { state: visit.historyState ?? null },
    performance: { getEntriesByType: (type: string) => (type === 'navigation' ? [{ type: visit.navigationType }] : []) },
    sessionStorage: {
      getItem(key: string) {
        guardStorage();
        return storage.get(key) ?? null;
      },
      removeItem(key: string) {
        guardStorage();
        storage.delete(key);
      },
    },
    document: {
      wasDiscarded: visit.wasDiscarded ?? false,
      documentElement: { classList: { add() {}, contains: () => false } },
      getElementById: () => null,
    },
    navigator: { userAgent: '', maxTouchPoints: 0 },
  };
  for (const [, source] of html.matchAll(/<script>([\s\S]*?)<\/script>/g)) {
    runInNewContext(source, context);
    if (stopped) break;
  }
  if (!stopped) for (const fn of timers) fn();
  return {
    replaced,
    stopped,
    href: location.href,
    stored: () => storage.get(HARNESS_REJOIN_SESSION_KEY) ?? null,
    // Back to this page out of the back-forward cache: no script runs again.
    restoreFromBackForwardCache: () => {
      for (const listener of listeners.get('pageshow') ?? []) listener({ persisted: true });
    },
  };
}

test('reloading or restoring the tab that was in this meeting goes back into the web app, never to the desktop app', async () => {
  const body = (await call('GET', { label: 'design-review', code: 'abc-defg-hjk' })).body as string;
  const rejoinScript = body.indexOf('sessionStorage');
  assert.ok(rejoinScript > -1 && rejoinScript < body.indexOf('<body>'), 'decided in <head>, before the invite page paints');

  for (const visit of [
    { navigationType: 'reload', stored: 'abc-defg-hjk' },
    // Chrome restoring a discarded tab reports back_forward, but says so.
    { navigationType: 'back_forward', wasDiscarded: true, stored: 'abc-defg-hjk' },
    // Safari/Firefox session restore: back_forward, but on the meeting's guard entry.
    { navigationType: 'back_forward', historyState: GUARD_STATE, stored: 'abc-defg-hjk' },
  ] satisfies RejoinVisit[]) {
    const result = visitInvitePage(body, visit);
    assert.deepEqual(result.replaced, ['/?code=abc-defg-hjk'], JSON.stringify(visit));
    assert.equal(result.stopped, true, 'the rest of the page never runs');
    assert.equal(result.href, 'https://meet.petal.live/design-review/abc-defg-hjk', 'no petal:// hand-off');
  }
});

test('the invite page stays the invite page for a new visit, a Back, another meeting, or no storage', async () => {
  const body = (await call('GET', { label: 'design-review', code: 'abc-defg-hjk' })).body as string;

  for (const visit of [
    { navigationType: 'navigate', stored: 'abc-defg-hjk' }, // the link opened again, e.g. from chat
    { navigationType: 'back_forward', stored: 'abc-defg-hjk' }, // Back with no guard: the user wanted out
    { navigationType: 'reload', stored: 'def-ghjk-mnp' }, // this tab was in another meeting
    { navigationType: 'reload', stored: 'ABC-DEFG-HJK' }, // exactly what the client writes, nothing looser
    { navigationType: 'reload', stored: null }, // a new tab, or the user left
    { navigationType: 'reload', historyState: GUARD_STATE, stored: null },
    { navigationType: 'reload', storageThrows: true },
  ] satisfies RejoinVisit[]) {
    const result = visitInvitePage(body, visit);
    assert.deepEqual(result.replaced, [], JSON.stringify(visit));
    assert.equal(result.stopped, false, JSON.stringify(visit));
    assert.equal(result.href, 'petal://join/abc-defg-hjk', `${JSON.stringify(visit)}: the page carries on as before`);
  }
});

test('backing out of the meeting to this page forgets it, so a later reload shows the invite page', async () => {
  const body = (await call('GET', { label: 'design-review', code: 'abc-defg-hjk' })).body as string;

  const back = visitInvitePage(body, { navigationType: 'back_forward', stored: 'abc-defg-hjk' });
  assert.equal(back.stored(), null, 'Back (not a restore) clears the tab\'s meeting');
  assert.deepEqual(visitInvitePage(body, { navigationType: 'reload', stored: back.stored() }).replaced, []);

  const fresh = visitInvitePage(body, { navigationType: 'navigate', stored: 'abc-defg-hjk' });
  assert.equal(fresh.stored(), 'abc-defg-hjk', 'opening the link again leaves the meeting tab alone');
  fresh.restoreFromBackForwardCache();
  assert.equal(fresh.stored(), null, 'Back into this page from the back-forward cache clears it too');
});

test('the rejoin target is this origin and this page\'s own normalized code', async () => {
  const body = (await call('GET', undefined, {}, '/ABC-DEFG-HJK')).body as string;

  assert.deepEqual(visitInvitePage(body, { navigationType: 'reload', stored: 'abc-defg-hjk' }).replaced, ['/?code=abc-defg-hjk']);
  assert.doesNotMatch(body, /Set-Cookie|document\.cookie/i, 'nothing leaves this tab');
});
