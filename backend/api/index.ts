// GET / -> this project is app.petal.live, a pure API host (token/rooms/
// admin/updater/download). The marketing page lives at petal.live
// (a separate repo/project, petal-website); the browser SPA + join links
// live at meet.petal.live (web-harness). A human landing here directly is
// almost certainly looking for one of those, so just send them to the
// marketing site rather than serving JSON or a bare 404.

import type { VercelRequest, VercelResponse } from '@vercel/node';
import { applyCors } from '../lib/http.js';

export default async function handler(req: VercelRequest, res: VercelResponse) {
  if (applyCors(req, res)) return;
  if (req.method !== 'GET') {
    res.status(405).json({ error: 'method not allowed' });
    return;
  }
  res.setHeader('Location', 'https://petal.live/');
  res.status(302).end();
}
