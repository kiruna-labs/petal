import type { VercelRequest, VercelResponse } from './vercel.js';
import { LiveKitConfigError } from './livekit.js';
import { GeminiConfigError } from './gemini.js';
import { captureApiError, errorTypeName, flushSentry } from './sentry.js';

const DEFAULT_ALLOWED_ORIGINS = [
  'https://app.petal.live',
  'https://meet.petal.live',
];

function headerValue(value: string | string[] | undefined): string | undefined {
  return Array.isArray(value) ? value[0] : value;
}

function configuredAllowedOrigins(): Set<string> {
  const configured = process.env.PETAL_ALLOWED_ORIGINS;
  const origins = configured
    ? configured
        .split(',')
        .map((origin) => origin.trim())
        .filter(Boolean)
    : DEFAULT_ALLOWED_ORIGINS;
  return new Set(origins);
}

function isAllowedOrigin(origin: string): boolean {
  if (/^https?:\/\/(localhost|127\.0\.0\.1)(:\d+)?$/.test(origin)) return true;
  return configuredAllowedOrigins().has(origin);
}

// Every rate-limit bucket in this codebase (including the ai-token minting
// cap) anchors on this key. Trust assumption: Vercel's edge strips/re-sets
// x-forwarded-for, so a client cannot spoof it as deployed today. That
// assumption breaks silently -- full rate-limit bypass, unbounded billable
// token minting -- if Petal is ever put behind another proxy/CDN/WAF that
// passes through a client-supplied XFF unmodified. Re-verify before adding
// any additional layer in front of this deployment.
export function clientRateLimitKey(req: VercelRequest): string {
  const forwarded = headerValue(req.headers['x-forwarded-for']);
  const firstForwarded = forwarded?.split(',')[0]?.trim();
  return (
    firstForwarded ||
    headerValue(req.headers['x-real-ip']) ||
    req.socket?.remoteAddress ||
    'unknown'
  );
}

// Restrictive CORS for browser callers. Native app / server-to-server requests
// usually carry no Origin header, so they are allowed without emitting a
// wildcard. Returns true if the request was fully handled here.
export function applyCors(req: VercelRequest, res: VercelResponse): boolean {
  const origin = headerValue(req.headers.origin);
  if (origin) {
    if (!isAllowedOrigin(origin)) {
      res.status(403).json({ error: 'origin not allowed' });
      return true;
    }
    res.setHeader('Access-Control-Allow-Origin', origin);
    res.setHeader('Vary', 'Origin');
  }
  res.setHeader('Access-Control-Allow-Methods', 'GET, POST, OPTIONS');
  // Authorization is required by /api/ai-token (the caller's LiveKit access
  // token) and /api/admin. Without it here, a browser preflight strips the
  // header and every cross-origin call from meet.petal.live fails (#655).
  res.setHeader('Access-Control-Allow-Headers', 'Content-Type, Authorization');
  if (req.method === 'OPTIONS') {
    res.status(204).end();
    return true;
  }
  return false;
}

function statusFromError(err: unknown): number | undefined {
  const status = (err as { status?: unknown })?.status;
  return typeof status === 'number' && Number.isInteger(status) ? status : undefined;
}

function messageFromError(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

// Classifies `err`, sends the matching response, and — for 5xx/unknown
// errors only, never 4xx passthrough (cost guardrail: routine bad room
// codes/expired tokens aren't exceptional) — reports it to Sentry via the
// allowlisted-fields-only path in lib/sentry.ts.
//
// MUST be async and every call site MUST `await` it: Vercel freezes the
// serverless function immediately once the handler's promise resolves, so
// the trailing `await flushSentry(2000)` below only actually flushes queued
// Sentry events if callers await this function all the way through. A
// fire-and-forget call here silently drops most events in production while
// every local/test run looks fine (#282).
export async function sendApiError(
  res: VercelResponse,
  err: unknown,
  context: { operation: string; fallbackStatus?: number; fallbackMessage?: string }
): Promise<void> {
  let capturedStatus: number | undefined;

  if (err instanceof LiveKitConfigError) {
    console.error(`${context.operation} failed: ${err.message}`);
    res.status(503).json({ error: 'LiveKit not configured' });
    capturedStatus = 503;
  } else if (err instanceof GeminiConfigError) {
    // The documented AI-chat kill switch (#655): unset GEMINI_API_KEY and
    // every mint returns this specific 503, which clients render as
    // "AI chat temporarily unavailable" rather than a generic failure (#656).
    console.error(`${context.operation} failed: ${err.message}`);
    res.status(503).json({ error: 'AI chat is not configured' });
    capturedStatus = 503;
  } else if (err instanceof SyntaxError) {
    console.error(`${context.operation} failed: invalid JSON body`, err);
    res.status(400).json({ error: 'invalid JSON body' });
    // 4xx — no Sentry capture.
  } else {
    const status = statusFromError(err);
    if (status !== undefined && status >= 400 && status < 500) {
      res.status(status).json({ error: messageFromError(err) });
      // 4xx passthrough — no Sentry capture.
    } else if (status !== undefined && status >= 500 && status < 600) {
      console.error(`${context.operation} failed: ${messageFromError(err)}`, err);
      res.status(status).json({ error: messageFromError(err) });
      capturedStatus = status;
    } else {
      console.error(`${context.operation} failed: ${messageFromError(err)}`, err);
      const fallbackStatus = context.fallbackStatus ?? 502;
      res.status(fallbackStatus).json({
        error: context.fallbackMessage ?? 'backend dependency failed',
      });
      capturedStatus = fallbackStatus;
    }
  }

  if (capturedStatus !== undefined) {
    await captureApiError(err, {
      operation: context.operation,
      route: context.operation,
      statusCode: capturedStatus,
      errorType: errorTypeName(err),
    });
  }
  await flushSentry(2000);
}
