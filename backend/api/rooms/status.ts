// POST /api/rooms/status { rooms: [{ room, accessCode? }] }
//   -> { rooms: [{ id, name, open, occupancy }] }
// Proof-of-possession room status: returns the directory view ONLY for rooms
// whose credential the caller holds (plus the access code for closed rooms).
// Replaces the public `GET /api/rooms` directory, which enumerated every
// room's name and headcount to anyone (docs/CONTRACTS.md "Room status").

import type { VercelRequest, VercelResponse } from '@vercel/node';
import { handleRoomStatus } from '../../lib/handlers.js';
import { applyCors, clientRateLimitKey, sendApiError } from '../../lib/http.js';

export default async function handler(req: VercelRequest, res: VercelResponse) {
  if (applyCors(req, res)) return;
  if (req.method !== 'POST') {
    res.status(405).json({ error: 'method not allowed' });
    return;
  }
  try {
    const body = typeof req.body === 'string' ? JSON.parse(req.body || '{}') : req.body ?? {};
    res.status(200).json(await handleRoomStatus(body, { rateLimitKey: clientRateLimitKey(req) }));
  } catch (err) {
    await sendApiError(res, err, {
      operation: '/api/rooms/status POST',
      fallbackStatus: 502,
      fallbackMessage: 'room service unavailable',
    });
  }
}
