// POST /api/rooms { name, open?, room? } -> { room }
//   Generates a credential, or stamps an existing one while preserving its
//   server-side open flag if metadata already exists.
// GET  /api/rooms -> 410 Gone. The public directory was removed: it listed
//   every room's name and headcount to anyone on the internet, and the
//   cross-machine discovery it served (#98/#155) had been inert since #83.
//   Clients that hold credentials use POST /api/rooms/status instead. 410 (not
//   404/405) so a stale client fails loudly with the reason.

import type { VercelRequest, VercelResponse } from '../lib/vercel.js';
import { handleCreateRoom } from '../lib/handlers.js';
import { applyCors, clientRateLimitKey, sendApiError } from '../lib/http.js';

export default async function handler(req: VercelRequest, res: VercelResponse) {
  if (applyCors(req, res)) return;
  try {
    if (req.method === 'GET') {
      res.status(410).json({ error: 'room directory removed; use POST /api/rooms/status' });
      return;
    }
    if (req.method === 'POST') {
      const body = typeof req.body === 'string' ? JSON.parse(req.body || '{}') : req.body ?? {};
      res.status(200).json(await handleCreateRoom(body, { rateLimitKey: clientRateLimitKey(req) }));
      return;
    }
    res.status(405).json({ error: 'method not allowed' });
  } catch (err) {
    await sendApiError(res, err, {
      operation: `/api/rooms ${req.method ?? 'UNKNOWN'}`,
      fallbackStatus: 502,
      fallbackMessage: 'room service unavailable',
    });
  }
}
