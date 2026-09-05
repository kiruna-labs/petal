// POST /api/ai-token  { room, identity }  + Authorization: Bearer <livekit jwt>
//   -> { token, requestedExpireTime, model, expireTime? }
// Mints a short-lived, single-use Gemini Live ephemeral token for one live
// meeting participant (#655). Thin Vercel adapter over handleAiToken; the
// two-layer auth story (JWT proof of identity + live-participant liveness),
// the rate buckets, and the GEMINI_API_KEY kill switch all live in that
// function's doc comment in lib/handlers.ts.
//
// Clients MUST use the `model` from the response rather than their own
// constant, so a preview-model rename is an env change here, not a release.
//
// EVERY SUCCESSFUL CALL COSTS REAL MONEY, and the call is not idempotent.
// Clients MUST allow AI_TOKEN_CLIENT_ATTEMPT_TIMEOUT_MS for a single attempt
// and MUST NOT retry on timeout or 5xx — a retried mint is a second billable
// token, never a second chance at the first one.
//
// `expireTime` is present only when Google reported one on the created token;
// `requestedExpireTime` is the ceiling we asked for. The two are never
// conflated: the requested value is not evidence of what Google granted.

import type { VercelRequest, VercelResponse } from '@vercel/node';
import { handleAiToken } from '../lib/handlers.js';
import { applyCors, clientRateLimitKey, sendApiError } from '../lib/http.js';

export default async function handler(req: VercelRequest, res: VercelResponse) {
  if (applyCors(req, res)) return;
  if (req.method !== 'POST') {
    res.status(405).json({ error: 'method not allowed' });
    return;
  }
  try {
    const body = typeof req.body === 'string' ? JSON.parse(req.body || '{}') : req.body ?? {};
    const authorization = Array.isArray(req.headers.authorization)
      ? req.headers.authorization[0]
      : req.headers.authorization;
    res.status(200).json(
      await handleAiToken(body, {
        authorization,
        rateLimitKey: clientRateLimitKey(req),
      })
    );
  } catch (err) {
    // GeminiConfigError -> 503 "AI chat is not configured" (the kill switch)
    // is handled inside sendApiError alongside LiveKitConfigError; the
    // fallback below stays the ordinary unexpected-failure 502.
    await sendApiError(res, err, {
      operation: '/api/ai-token POST',
      fallbackStatus: 502,
      fallbackMessage: 'ai token service unavailable',
    });
  }
}
