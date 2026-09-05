// GET /api/version -> the commit this deployment was built from.
//
// Backend and web-harness deploy separately from `git push` (a plain
// `vercel --prod`, see docs/RELEASING.md); nothing else here proves a
// deploy actually picked up the latest `main`. This has bitten the project
// twice already (see verify-backend-live.sh / verify-web-harness-live.sh
// headers). `scripts/verify-deploy-freshness.sh` reads this endpoint and
// fails loudly if the live commit is missing or stale, instead of relying
// on someone noticing.
//
// PETAL_DEPLOY_COMMIT is set at deploy time via `vercel --prod -e
// PETAL_DEPLOY_COMMIT=$(git rev-parse HEAD)` (see docs/RELEASING.md). It is
// intentionally a runtime env var, not build-time: Vercel's Node function
// builder does not guarantee build-time vars are still readable from
// `process.env` at request time for a zero-config function.

import type { VercelRequest, VercelResponse } from '@vercel/node';
import { applyCors } from '../lib/http.js';

export default async function handler(req: VercelRequest, res: VercelResponse) {
  if (applyCors(req, res)) return;
  if (req.method !== 'GET') {
    res.status(405).json({ error: 'method not allowed' });
    return;
  }
  res.status(200).json({ commit: process.env.PETAL_DEPLOY_COMMIT || null });
}
