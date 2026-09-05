// Moved from backend/test/distribution.ts alongside api/j.ts itself (see
// api/j.ts's header comment) -- join links now live at meet.petal.live.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import type { VercelRequest, VercelResponse } from '@vercel/node';
import joinHandler, {
  desktopDownloadPlatformForUserAgent,
  downloadUrlForPlatform,
  webJoinUrlForAccessCode,
} from '../api/j.ts';

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
  assert.match(body, /class="brand-mark"/);
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
