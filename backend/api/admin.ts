// POST /api/admin { action: "kick" | "close", room, identity? }
// Admin-only LiveKit control primitives for revoking a participant or closing
// a room. The bearer token is a server/admin secret, never a room credential.

import type { VercelRequest, VercelResponse } from '../lib/vercel.js';
import { handleAdminControl } from '../lib/handlers.js';
import { applyCors, sendApiError } from '../lib/http.js';

export default async function handler(req: VercelRequest, res: VercelResponse) {
  if (applyCors(req, res)) return;
  if (req.method !== 'POST') {
    res.status(405).json({ error: 'method not allowed' });
    return;
  }
  try {
    const body = typeof req.body === 'string' ? JSON.parse(req.body || '{}') : req.body ?? {};
    res.status(200).json(
      await handleAdminControl(body, {
        authorization: typeof req.headers.authorization === 'string' ? req.headers.authorization : undefined,
      })
    );
  } catch (err) {
    // #282: previously a bespoke inline catch with ZERO console logging on
    // unexpected failures — routed through sendApiError so admin failures are
    // now logged + Sentry-reported like every other route. HttpError-driven
    // statuses (400/401/403) are byte-identical to before.
    await sendApiError(res, err, {
      operation: '/api/admin POST',
      fallbackStatus: 502,
      fallbackMessage: 'admin control unavailable',
    });
  }
}
