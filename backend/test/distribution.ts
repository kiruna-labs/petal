import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { getGlobalDispatcher, MockAgent, setGlobalDispatcher } from 'undici';
import type { VercelRequest, VercelResponse } from '../lib/vercel.js';
import adminHandler from '../api/admin.js';
import downloadHandler from '../api/download.js';
import galleryTokenHandler from '../api/gallery-token.js';
import indexHandler from '../api/index.js';
import roomsHandler from '../api/rooms.js';
import tokenHandler from '../api/token.js';
import updaterHandler from '../api/updater.js';
import { ACCESS_CODE_ALPHABET, credentialForAccessCode, generateAccessCode, normalizeAccessCode } from '../lib/slug.js';
import {
  findBlobByPathname,
  findBlobByPrefixSuffix,
} from '../lib/blob.js';
import { sendApiError } from '../lib/http.js';

type BlobJson = {
  url: string;
  downloadUrl: string;
  pathname: string;
  size: number;
  uploadedAt: string;
  etag: string;
};

const contractFixture = JSON.parse(
  readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8')
) as {
  inviteLinks: Array<{
    label: string;
    accessCode: string;
    credential: string;
    httpsPath: string;
    nativeDeepLink: string;
    webJoinQuery: string;
  }>;
  pipelineStatsMessages: Array<{
    fields: string[];
    captureStateFields: string[];
    captureCpuFields: string[];
    receiverFreezeFields: string[];
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

const originalDispatcher = getGlobalDispatcher();
const originalFetch = globalThis.fetch;
const blobApi = new MockAgent();
const pool = blobApi.get('https://vercel.com');
let manifestByUrl = new Map<string, unknown>();

process.env.BLOB_READ_WRITE_TOKEN = 'vercel_blob_rw_teststore_secret';
blobApi.disableNetConnect();
setGlobalDispatcher(blobApi);

globalThis.fetch = async (input: string | URL | Request) => {
  const url = input instanceof Request ? input.url : String(input);
  if (!manifestByUrl.has(url)) {
    throw new Error(`unexpected fetch: ${url}`);
  }
  return Response.json(manifestByUrl.get(url));
};

function blob(pathname: string, uploadedAt: string): BlobJson {
  const url = `https://cdn.example/${pathname}`;
  return {
    url,
    downloadUrl: `${url}?download=1`,
    pathname,
    size: 123,
    uploadedAt,
    etag: `"${pathname}"`,
  };
}

function listBlobs(prefix: string, blobs: BlobJson[]) {
  pool
    .intercept({
      method: 'GET',
      path: `/api/blob?prefix=${encodeURIComponent(prefix)}`,
    })
    .reply(200, { blobs, hasMore: false });
}

function req(method: string, headers: Record<string, string> = {}): VercelRequest {
  return { method, headers } as unknown as VercelRequest;
}

function reqWithBody(
  method: string,
  body: unknown,
  headers: Record<string, string> = {}
): VercelRequest {
  return { method, body, headers } as unknown as VercelRequest;
}

function reqWithQuery(
  method: string,
  query: Record<string, string>,
  headers: Record<string, string> = {}
): VercelRequest {
  return { method, query, headers } as unknown as VercelRequest;
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
  handler: (req: VercelRequest, res: VercelResponse) => Promise<void>,
  method = 'GET',
  query?: Record<string, string>,
  headers: Record<string, string> = {}
): Promise<TestResponse> {
  const response = res();
  await handler(
    query ? reqWithQuery(method, query, headers) : req(method, headers),
    response as unknown as VercelResponse
  );
  return response;
}

async function callWithBody(
  handler: (req: VercelRequest, res: VercelResponse) => Promise<void>,
  method: string,
  body: unknown,
  headers: Record<string, string> = {}
): Promise<TestResponse> {
  const response = res();
  await handler(reqWithBody(method, body, headers), response as unknown as VercelResponse);
  return response;
}

async function captureConsoleError(fn: () => Promise<void> | void): Promise<string[]> {
  const messages: string[] = [];
  const original = console.error;
  console.error = (...args: unknown[]) => {
    messages.push(args.map(String).join(' '));
  };
  try {
    await fn();
  } finally {
    console.error = original;
  }
  return messages;
}

async function withMissingLiveKitEnv(fn: () => Promise<void>) {
  const saved = {
    LIVEKIT_URL: process.env.LIVEKIT_URL,
    LIVEKIT_API_KEY: process.env.LIVEKIT_API_KEY,
    LIVEKIT_API_SECRET: process.env.LIVEKIT_API_SECRET,
  };
  delete process.env.LIVEKIT_URL;
  delete process.env.LIVEKIT_API_KEY;
  delete process.env.LIVEKIT_API_SECRET;
  try {
    await fn();
  } finally {
    if (saved.LIVEKIT_URL === undefined) delete process.env.LIVEKIT_URL;
    else process.env.LIVEKIT_URL = saved.LIVEKIT_URL;
    if (saved.LIVEKIT_API_KEY === undefined) delete process.env.LIVEKIT_API_KEY;
    else process.env.LIVEKIT_API_KEY = saved.LIVEKIT_API_KEY;
    if (saved.LIVEKIT_API_SECRET === undefined) delete process.env.LIVEKIT_API_SECRET;
    else process.env.LIVEKIT_API_SECRET = saved.LIVEKIT_API_SECRET;
  }
}

async function withMissingBlobToken(fn: () => Promise<void>) {
  const saved = process.env.BLOB_READ_WRITE_TOKEN;
  delete process.env.BLOB_READ_WRITE_TOKEN;
  try {
    await fn();
  } finally {
    if (saved === undefined) delete process.env.BLOB_READ_WRITE_TOKEN;
    else process.env.BLOB_READ_WRITE_TOKEN = saved;
  }
}

let failures = 0;
async function test(name: string, fn: () => Promise<void> | void) {
  try {
    manifestByUrl = new Map();
    await fn();
    console.log(`  ok   ${name}`);
  } catch (err) {
    failures++;
    console.error(`  FAIL ${name}`);
    console.error(err);
  }
}

async function main() {
  console.log('auto-update distribution endpoints:');

  await test('generated access codes match native/web 3-4-3 lockstep', () => {
    assert.equal(ACCESS_CODE_ALPHABET, 'abcdefghjkmnopqrstuvwxyz');
    const generated = Array.from({ length: 200 }, () => generateAccessCode());
    assert.ok(generated.every((code) => /^[a-hjkm-z]{3}-[a-hjkm-z]{4}-[a-hjkm-z]{3}$/.test(code)));
    assert.equal(normalizeAccessCode('myq-xfkw-azrp'), null);
  });

  await test('pipeline-stats fixture pins capture and freeze report fields', () => {
    for (const vector of contractFixture.pipelineStatsMessages) {
      assert.deepEqual(vector.fields, [
        'captureState',
        'decoded',
        'encodedSent',
        'grabbed',
        'lifecycle',
        'ownerIdentity',
        'publicationSid',
        'received',
        'receiverFreeze',
        'reporterId',
        'role',
        'sentAtMs',
        'seq',
        'shareEpoch',
        'v',
        'windowId',
      ]);
      assert.deepEqual(vector.captureStateFields, [
        'cpu',
        'dirtyAreaPx',
        'dirtyRectCount',
        'fps',
        'occlusionPct',
        'state',
      ]);
      assert.deepEqual(vector.captureCpuFields, [
        'captureFrameReturnMs',
        'convertMs',
        'lockCopyMs',
      ]);
      assert.deepEqual(vector.receiverFreezeFields, [
        'framesDropped',
        'freezeCount',
        'qualityLimitationReason',
      ]);
    }
  });

  await test('/api/updater returns latest.json manifest verbatim', async () => {
    const latest = blob('latest.json', '2026-07-01T10:00:00.000Z');
    const manifest = {
      version: '1.2.3',
      notes: 'Release notes',
      pub_date: '2026-07-01T10:00:00Z',
      platforms: {
        'darwin-universal': {
          signature: 'abc',
          url: 'https://cdn.example/Petal_universal.app.tar.gz',
        },
      },
    };
    manifestByUrl.set(latest.url, manifest);
    listBlobs('latest.json', [latest]);

    const response = await call(updaterHandler);

    assert.equal(response.statusCode, 200);
    assert.deepEqual(response.body, manifest);
    assert.equal(response.headers['content-type'], 'application/json');
    assert.equal(response.headers['access-control-allow-origin'], undefined);
  });

  await test('/api/updater returns 204 when latest.json is absent', async () => {
    listBlobs('latest.json', [blob('latest.json.old', '2026-07-01T10:00:00.000Z')]);

    const response = await call(updaterHandler);

    assert.equal(response.statusCode, 204);
    assert.equal(response.ended, true);
  });

  await test('/api/updater returns 204 when latest.json fetch fails', async () => {
    const latest = blob('latest.json', '2026-07-01T10:00:00.000Z');
    listBlobs('latest.json', [latest]);

    const response = await call(updaterHandler);

    assert.equal(response.statusCode, 204);
    assert.equal(response.ended, true);
  });

  await test('/api/updater handles OPTIONS CORS preflight', async () => {
    const response = await call(updaterHandler, 'OPTIONS', undefined, {
      origin: 'https://app.petal.live',
    });

    assert.equal(response.statusCode, 204);
    assert.equal(response.ended, true);
    assert.equal(response.headers['access-control-allow-origin'], 'https://app.petal.live');
    assert.equal(response.headers.vary, 'Origin');
    assert.equal(response.headers['access-control-allow-methods'], 'GET, POST, OPTIONS');
    // Authorization is allowlisted for /api/ai-token and /api/admin — without
    // it a browser preflight strips the header entirely (#655).
    assert.equal(response.headers['access-control-allow-headers'], 'Content-Type, Authorization');
  });

  await test('CORS rejects disallowed browser origins before token/rooms handlers run', async () => {
    for (const handler of [tokenHandler, roomsHandler]) {
      const response = await call(handler, 'OPTIONS', undefined, {
        origin: 'https://evil.example',
      });
      assert.equal(response.statusCode, 403);
      assert.deepEqual(response.body, { error: 'origin not allowed' });
      assert.equal(response.headers['access-control-allow-origin'], undefined);
    }
  });

  await test('/api/rooms GET is 410 Gone (public directory removed)', async () => {
    const response = await callWithBody(roomsHandler, 'GET', undefined);
    assert.equal(response.statusCode, 410);
    assert.deepEqual(response.body, { error: 'room directory removed; use POST /api/rooms/status' });
  });

  await test('/api/rooms returns a diagnosable error when LiveKit env is missing', async () => {
    await withMissingLiveKitEnv(async () => {
      let response!: TestResponse;
      const logs = await captureConsoleError(async () => {
        response = await callWithBody(roomsHandler, 'POST', { name: 'Eng Sync' });
      });

      assert.equal(response.statusCode, 503);
      assert.deepEqual(response.body, { error: 'LiveKit not configured' });
      assert.match(logs.join('\n'), /\/api\/rooms POST failed: LiveKit not configured/);
      assert.match(logs.join('\n'), /LIVEKIT_URL, LIVEKIT_API_KEY, LIVEKIT_API_SECRET/);
    });
  });

  await test('/api/token returns a diagnosable error when LiveKit env is missing', async () => {
    await withMissingLiveKitEnv(async () => {
      const [vector] = contractFixture.inviteLinks;
      let response!: TestResponse;
      const logs = await captureConsoleError(async () => {
        response = await callWithBody(tokenHandler, 'POST', {
          room: vector.credential,
          identity: '11111111-1111-4111-8111-111111111111',
        });
      });

      assert.equal(response.statusCode, 503);
      assert.deepEqual(response.body, { error: 'LiveKit not configured' });
      assert.match(logs.join('\n'), /\/api\/token POST failed: LiveKit not configured/);
      assert.match(logs.join('\n'), /LIVEKIT_URL, LIVEKIT_API_KEY, LIVEKIT_API_SECRET/);
    });
  });

  // #282: admin.ts, gallery-token.ts, and download.ts used to have their own
  // bespoke inline catch blocks with ZERO console logging on unexpected
  // failures. They now route through the shared sendApiError. These tests
  // pin the response contract across that refactor: every status/body shape
  // driven by an HttpError is byte-identical to before; the previously-buggy
  // "unknown error" cases (which used to leak the raw error message at a
  // bespoke status) now deliberately match the same classified shape every
  // other route already gives (LiveKitConfigError -> 503 generic message,
  // truly-unknown -> the route's fallbackStatus/fallbackMessage).

  await test('/api/admin rejects missing/invalid admin authorization with the pre-existing response shape', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aap')!;
    const savedToken = process.env.PETAL_ADMIN_TOKEN;
    try {
      delete process.env.PETAL_ADMIN_TOKEN;
      let response = await callWithBody(adminHandler, 'POST', { action: 'close', room: credential });
      assert.equal(response.statusCode, 503);
      assert.deepEqual(response.body, { error: 'admin control is not configured' });

      process.env.PETAL_ADMIN_TOKEN = 'admin-secret';
      response = await callWithBody(adminHandler, 'POST', { action: 'close', room: credential });
      assert.equal(response.statusCode, 401);
      assert.deepEqual(response.body, { error: 'admin authorization is required' });

      response = await callWithBody(adminHandler, 'POST', { action: 'close', room: credential }, {
        authorization: 'Bearer wrong',
      });
      assert.equal(response.statusCode, 403);
      assert.deepEqual(response.body, { error: 'admin authorization failed' });
    } finally {
      if (savedToken === undefined) delete process.env.PETAL_ADMIN_TOKEN;
      else process.env.PETAL_ADMIN_TOKEN = savedToken;
    }
  });

  await test('/api/admin surfaces LiveKit misconfiguration as the same 503 every other route already gives (was 500 + raw message)', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aaq')!;
    const savedToken = process.env.PETAL_ADMIN_TOKEN;
    process.env.PETAL_ADMIN_TOKEN = 'admin-secret';
    try {
      await withMissingLiveKitEnv(async () => {
        const response = await callWithBody(
          adminHandler,
          'POST',
          { action: 'close', room: credential },
          { authorization: 'Bearer admin-secret' }
        );
        assert.equal(response.statusCode, 503);
        assert.deepEqual(response.body, { error: 'LiveKit not configured' });
      });
    } finally {
      if (savedToken === undefined) delete process.env.PETAL_ADMIN_TOKEN;
      else process.env.PETAL_ADMIN_TOKEN = savedToken;
    }
  });

  await test('/api/gallery-token rejects malformed requests with the pre-existing response shape', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aar')!;
    const response = await callWithBody(galleryTokenHandler, 'POST', { room: credential });
    assert.equal(response.statusCode, 400);
    assert.deepEqual(response.body, { error: 'baseIdentity is required' });
  });

  await test('/api/gallery-token surfaces LiveKit misconfiguration as the same 503 every other route already gives (was 500 + raw message)', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aas')!;
    await withMissingLiveKitEnv(async () => {
      const response = await callWithBody(galleryTokenHandler, 'POST', {
        room: credential,
        baseIdentity: '11111111-1111-4111-8111-111111111111',
      });
      assert.equal(response.statusCode, 503);
      assert.deepEqual(response.body, { error: 'LiveKit not configured' });
    });
  });

  await test('/api/download unexpected failure surfaces the shared fallback (was 500 + raw message leak)', async () => {
    await withMissingBlobToken(async () => {
      const response = await call(downloadHandler);
      assert.equal(response.statusCode, 502);
      assert.deepEqual(response.body, { error: 'download service unavailable' });
    });
  });

  await test('sendApiError fallback contract is pinned per-route (admin/gallery-token/download)', async () => {
    const unexpected = new Error('some internal detail that must never reach the caller');
    for (const [operation, fallbackMessage] of [
      ['/api/admin POST', 'admin control unavailable'],
      ['/api/gallery-token POST', 'gallery token service unavailable'],
      ['/api/download GET', 'download service unavailable'],
    ] as const) {
      const response = res();
      await sendApiError(response as unknown as VercelResponse, unexpected, {
        operation,
        fallbackStatus: 502,
        fallbackMessage,
      });
      assert.equal(response.statusCode, 502);
      assert.deepEqual(response.body, { error: fallbackMessage });
    }
  });

  await test('/api/download redirects to the newest universal DMG blob by default', async () => {
    const oldDmg = blob('Petal_1.2.2_universal.dmg', '2026-06-30T10:00:00.000Z');
    const currentDmg = blob('Petal_1.2.3_universal.dmg', '2026-07-01T10:00:00.000Z');
    const ignoredZip = blob('Petal_1.2.3_universal.zip', '2026-07-02T10:00:00.000Z');
    listBlobs('Petal_', [oldDmg, ignoredZip, currentDmg]);

    const response = await call(downloadHandler);

    assert.equal(response.statusCode, 302);
    assert.equal(response.headers.location, currentDmg.url);
    assert.equal(response.ended, true);
  });

  await test('/api/download explicitly selects macOS without changing the artifact contract', async () => {
    const currentDmg = blob('Petal_1.2.3_universal.dmg', '2026-07-01T10:00:00.000Z');
    const windows = blob('Petal_1.2.3_windows_x86_64-setup.exe', '2026-07-02T10:00:00.000Z');
    listBlobs('Petal_', [currentDmg, windows]);

    const response = await call(downloadHandler, 'GET', { platform: 'macos' });

    assert.equal(response.statusCode, 302);
    assert.equal(response.headers.location, currentDmg.url);
  });

  await test('/api/download selects the newest Windows x86-64 NSIS installer', async () => {
    const oldInstaller = blob('Petal_1.2.2_windows_x86_64-setup.exe', '2026-06-30T10:00:00.000Z');
    const currentInstaller = blob('Petal_1.2.3_windows_x86_64-setup.exe', '2026-07-01T10:00:00.000Z');
    const ignoredMsi = blob('Petal_1.2.4_windows_x86_64.msi', '2026-07-02T10:00:00.000Z');
    listBlobs('Petal_', [oldInstaller, ignoredMsi, currentInstaller]);

    const response = await call(downloadHandler, 'GET', { platform: 'windows' });

    assert.equal(response.statusCode, 302);
    assert.equal(response.headers.location, currentInstaller.url);
    assert.equal(response.ended, true);
  });

  await test('/api/download rejects an unknown platform before listing blobs', async () => {
    const response = await call(downloadHandler, 'GET', { platform: 'linux' });

    assert.equal(response.statusCode, 400);
    assert.deepEqual(response.body, { error: 'platform must be macos or windows' });
  });

  await test('/api/download returns 404 when the requested platform artifact is missing', async () => {
    const currentDmg = blob('Petal_1.2.3_universal.dmg', '2026-07-01T10:00:00.000Z');
    listBlobs('Petal_', [currentDmg]);

    const response = await call(downloadHandler, 'GET', { platform: 'windows' });

    assert.equal(response.statusCode, 404);
    assert.deepEqual(response.body, { error: 'no release published yet' });
  });

  await test('/api/index redirects to the marketing site', async () => {
    const response = await call(indexHandler);

    // This project (app.petal.live) is a pure API host now -- the marketing
    // page lives at petal.live in the separate petal-website repo. A human
    // landing on the bare API root gets bounced there instead of JSON/404.
    assert.equal(response.statusCode, 302);
    assert.equal(response.headers['location'], 'https://petal.live/');
    assert.equal(response.ended, true);
  });

  await test('/api/index rejects non-GET methods', async () => {
    const response = await call(indexHandler, 'POST');

    assert.equal(response.statusCode, 405);
    assert.deepEqual(response.body, { error: 'method not allowed' });
  });

  // /<label>/<access-code> join-link interstitial tests moved to
  // web-harness/tests/j.test.ts alongside api/j.ts itself (join links now
  // live at meet.petal.live, not this project's domain).

  console.log('blob helpers:');

  await test('findBlobByPathname resolves an exact pathname match', async () => {
    const exact = blob('latest.json', '2026-07-01T10:00:00.000Z');
    listBlobs('latest.json', [blob('latest.json.bak', '2026-07-02T10:00:00.000Z'), exact]);

    const found = await findBlobByPathname('latest.json');

    assert.equal(found?.pathname, exact.pathname);
    assert.equal(found?.url, exact.url);
  });

  await test('findBlobByPrefixSuffix picks the newest matching suffix', async () => {
    const oldDmg = blob('Petal_1.2.2_universal.dmg', '2026-06-30T10:00:00.000Z');
    const newDmg = blob('Petal_1.2.3_universal.dmg', '2026-07-01T10:00:00.000Z');
    const ignored = blob('Petal_1.2.4_universal.zip', '2026-07-02T10:00:00.000Z');
    listBlobs('Petal_', [oldDmg, ignored, newDmg]);

    const found = await findBlobByPrefixSuffix('Petal_', '_universal.dmg');

    assert.equal(found?.pathname, newDmg.pathname);
    assert.equal(found?.url, newDmg.url);
  });

  await blobApi.assertNoPendingInterceptors();
  setGlobalDispatcher(originalDispatcher);
  globalThis.fetch = originalFetch;

  console.log('');
  if (failures === 0) {
    console.log('ALL PASSED');
  } else {
    console.error(`${failures} CHECK(S) FAILED`);
    process.exit(1);
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
