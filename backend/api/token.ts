// POST /api/token  { room, identity, displayName? }
//   -> { url, token, room }
// Thin Vercel adapter over handleToken. Browser callers are CORS-allowlisted;
// native/server callers normally send no Origin header.

import type { VercelRequest, VercelResponse } from '../lib/vercel.js';
import { handleToken } from '../lib/handlers.js';
import { applyCors, clientRateLimitKey, sendApiError } from '../lib/http.js';

export default async function handler(req: VercelRequest, res: VercelResponse) {
  if (applyCors(req, res)) return;
  if (req.method !== 'POST') {
    res.status(405).json({ error: 'method not allowed' });
    return;
  }
  try {
    const body = typeof req.body === 'string' ? JSON.parse(req.body || '{}') : req.body ?? {};
    res.status(200).json(await handleToken(body, { rateLimitKey: clientRateLimitKey(req) }));
  } catch (err) {
    await sendApiError(res, err, {
      operation: '/api/token POST',
      fallbackStatus: 502,
      fallbackMessage: 'token service unavailable',
    });
  }
}
