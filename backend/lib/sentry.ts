// Sentry error reporting — gated entirely on SENTRY_DSN. Absent env var means
// fully off (local dev/CI never reports, no network calls, no perf cost).
//
// PII POLICY (allowlist-first, #282): we NEVER forward a real error's raw
// `.message`/stack to Sentry, because a future sloppy call site could
// interpolate room/identity/token data into an Error message and there would
// be nothing left to catch it. Only a fixed, static, pre-approved set of
// fields ever leaves this process:
//   - `operation`  — the caller-supplied static string (e.g. "/api/token POST")
//   - `route`      — same static string, tagged for Sentry's issue grouping
//   - `statusCode` — the HTTP status we're about to respond with
//   - `errorType`  — the thrown value's constructor name (e.g. "TypeError")
// The event sent to Sentry is a SYNTHETIC error whose message is the static
// `operation` string, not the original error object. `captureApiError` must
// stay the only call site that talks to the Sentry SDK's `captureException`.
//
// tracesSampleRate is pinned to 0 — no perf-tracing product needed here.

import * as SentryNode from '@sentry/node';

type SentryLike = Pick<typeof SentryNode, 'init' | 'captureException' | 'flush'>;

let sentryClient: SentryLike = SentryNode;
let initialized = false;

// Test-only seam: swap in a spy/mock so tests can assert exactly what would
// have been sent to Sentry without making real network calls. Passing
// undefined restores the real SDK and clears the "already initialized" latch
// so the next call re-evaluates SENTRY_DSN.
export function _setSentryClientForTest(mock: SentryLike | undefined): void {
  sentryClient = mock ?? SentryNode;
  initialized = false;
}

function ensureInit(): boolean {
  const dsn = process.env.SENTRY_DSN;
  if (!dsn) return false;
  if (!initialized) {
    sentryClient.init({
      dsn,
      tracesSampleRate: 0,
    });
    initialized = true;
  }
  return true;
}

export interface ApiErrorTags {
  operation: string;
  route: string;
  statusCode: number;
  errorType: string;
}

// Report a server-side API failure to Sentry. Callers decide WHEN to call
// this (5xx/unknown only — never 4xx passthrough, a cost guardrail: routine
// bad room codes/expired tokens are not exceptional and shouldn't burn
// Sentry event quota). This function only ever sends the allowlisted fields
// above — never the original error's message or stack.
// `_err` is intentionally unused beyond typing the call sites — see the PII
// POLICY note above: the raw error's message/stack never leaves this
// process, so nothing is ever read off it here.
export async function captureApiError(_err: unknown, tags: ApiErrorTags): Promise<void> {
  if (!ensureInit()) return;
  const sanitized = new Error(tags.operation);
  sanitized.name = tags.errorType;
  sentryClient.captureException(sanitized, {
    tags: {
      operation: tags.operation,
      route: tags.route,
      statusCode: tags.statusCode,
      errorType: tags.errorType,
    },
  });
}

// Vercel freezes a serverless function's process immediately after its
// handler promise resolves — any Sentry event queued without an AWAITED
// flush before that point silently never leaves the process in production,
// even though local/test runs (where nothing freezes) look perfectly fine.
// Every call site that responds with an error MUST await this before
// returning. Absent SENTRY_DSN, this is a no-op (nothing was ever queued).
export async function flushSentry(timeoutMs = 2000): Promise<void> {
  if (!process.env.SENTRY_DSN) return;
  if (!initialized) return;
  await sentryClient.flush(timeoutMs);
}

export function errorTypeName(err: unknown): string {
  return err instanceof Error ? err.constructor.name : typeof err;
}
