// POST /api/gallery-token  { room, baseIdentity, displayName? }
//   -> { url, token, room }
// Trusted, server-owned path for the desktop app's hidden gallery-bridge
// participant (#109). Thin Vercel adapter over handleGalleryToken. See that
// function's doc comment in lib/handlers.ts for why the public /api/token
// endpoint cannot serve this (its hidden/grant clamp, #100, is intentional
// and must stay).

import type { VercelRequest, VercelResponse } from '@vercel/node';
import { handleGalleryToken } from '../lib/handlers.js';
import { applyCors, clientRateLimitKey, sendApiError } from '../lib/http.js';

export default async function handler(req: VercelRequest, res: VercelResponse) {
  if (applyCors(req, res)) return;
  if (req.method !== 'POST') {
    res.status(405).json({ error: 'method not allowed' });
    return;
  }
  try {
    const body = typeof req.body === 'string' ? JSON.parse(req.body || '{}') : req.body ?? {};
    res.status(200).json(await handleGalleryToken(body, { rateLimitKey: clientRateLimitKey(req) }));
  } catch (err) {
    // #282: previously a bespoke inline catch with ZERO console logging on
    // unexpected failures — routed through sendApiError so gallery-token
    // failures are now logged + Sentry-reported like every other route.
    // HttpError-driven statuses (400/403) are byte-identical to before.
    await sendApiError(res, err, {
      operation: '/api/gallery-token POST',
      fallbackStatus: 502,
      fallbackMessage: 'gallery token service unavailable',
    });
  }
}
