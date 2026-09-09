// Browser-origin policy for the backend API (#42 follow-up).
//
// The release e2e gate loads the Test Cockpit's web peers from the STAGED
// web-harness deployment and they mint room tokens from this backend. v0.9.10's
// gate failed with six INFRA-FAILs because that origin was refused with 403
// "origin not allowed" before any CORS header was written, so the browser only
// ever saw "blocked by CORS policy". These checks pin the allowance to exactly
// our own project's Vercel-issued origins and nothing that merely looks like one.

import assert from 'node:assert/strict';
import type { VercelRequest, VercelResponse } from '../lib/vercel.js';
import { applyCors, isAllowedOriginForTest } from '../lib/http.js';

let failures = 0;

function check(name: string, fn: () => void): void {
  try {
    fn();
    console.log(`  ok  ${name}`);
  } catch (err) {
    failures += 1;
    console.error(`  FAIL ${name}`);
    console.error(err);
  }
}

type FakeRes = {
  statusCode?: number;
  headers: Record<string, string>;
  body?: unknown;
  ended: boolean;
};

function res(): FakeRes & VercelResponse {
  const state: FakeRes = { headers: {}, ended: false };
  const api = {
    ...state,
    setHeader(key: string, value: string) {
      state.headers[key.toLowerCase()] = value;
      return api;
    },
    status(code: number) {
      state.statusCode = code;
      api.statusCode = code;
      return api;
    },
    json(payload: unknown) {
      state.body = payload;
      api.body = payload;
      return api;
    },
    end() {
      state.ended = true;
      api.ended = true;
      return api;
    },
  };
  return api as unknown as FakeRes & VercelResponse;
}

function req(origin?: string, method = 'POST'): VercelRequest {
  return {
    method,
    headers: origin ? { origin } : {},
  } as unknown as VercelRequest;
}

const STAGED = 'https://web-harness-3rhaasn8g-kiruna-labs.vercel.app';

function main(): void {
  console.log('cors: origin policy');

  check('the production web origins stay allowed', () => {
    assert.equal(isAllowedOriginForTest('https://meet.petal.live'), true);
    assert.equal(isAllowedOriginForTest('https://app.petal.live'), true);
  });

  check('localhost dev origins stay allowed', () => {
    assert.equal(isAllowedOriginForTest('http://localhost:5185'), true);
    assert.equal(isAllowedOriginForTest('http://127.0.0.1:5173'), true);
  });

  check('a staged web-harness deployment origin is allowed', () => {
    // The exact origin from release run 34275720644, which the gate refused.
    assert.equal(isAllowedOriginForTest(STAGED), true);
    assert.equal(
      isAllowedOriginForTest('https://web-harness-git-main-kiruna-labs.vercel.app'),
      true
    );
  });

  check('lookalike origins are still refused', () => {
    for (const origin of [
      // Suffix smuggling -- the regex must be anchored at both ends.
      'https://web-harness-3rhaasn8g-kiruna-labs.vercel.app.evil.example',
      'https://evil.example/web-harness-3rhaasn8g-kiruna-labs.vercel.app',
      // Wrong scheme, wrong team, wrong project, wrong apex.
      'http://web-harness-3rhaasn8g-kiruna-labs.vercel.app',
      'https://web-harness-3rhaasn8g-someone-else.vercel.app',
      'https://petal-backend-ixopapjly-kiruna-labs.vercel.app',
      'https://web-harness-3rhaasn8g-kiruna-labs.vercel.dev',
      // No deployment segment at all.
      'https://web-harness--kiruna-labs.vercel.app',
    ]) {
      assert.equal(isAllowedOriginForTest(origin), false, `should refuse ${origin}`);
    }
  });

  check('applyCors echoes an allowed staged origin instead of 403ing', () => {
    const response = res();
    const handled = applyCors(req(STAGED), response);
    assert.equal(handled, false, 'a POST must fall through to the handler');
    assert.equal(response.statusCode, undefined, 'no error status');
    assert.equal(response.headers['access-control-allow-origin'], STAGED);
    assert.equal(response.headers['vary'], 'Origin');
  });

  check('applyCors answers a staged-origin preflight with 204 + the headers', () => {
    const response = res();
    const handled = applyCors(req(STAGED, 'OPTIONS'), response);
    assert.equal(handled, true, 'a preflight is handled here');
    assert.equal(response.statusCode, 204);
    assert.equal(response.headers['access-control-allow-origin'], STAGED);
    assert.match(response.headers['access-control-allow-headers'], /Authorization/);
  });

  check('applyCors still refuses a disallowed origin with 403', () => {
    const response = res();
    const handled = applyCors(req('https://evil.example'), response);
    assert.equal(handled, true);
    assert.equal(response.statusCode, 403);
    assert.equal(response.headers['access-control-allow-origin'], undefined);
  });

  console.log('');
  if (failures === 0) {
    console.log('ALL PASSED');
  } else {
    console.error(`${failures} CHECK(S) FAILED`);
    process.exit(1);
  }
}

main();
