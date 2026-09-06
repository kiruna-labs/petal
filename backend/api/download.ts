// GET /api/download -> 302 redirect to the current desktop installer.
// Bare requests remain macOS-compatible; `?platform=macos|windows` selects a
// deterministic platform-specific artifact for websites and invite surfaces.

import type { VercelRequest, VercelResponse } from '../lib/vercel.js';
import { findBlobByPrefixSuffix } from '../lib/blob.js';
import { applyCors, sendApiError } from '../lib/http.js';

export default async function handler(req: VercelRequest, res: VercelResponse) {
  if (applyCors(req, res)) return;
  if (req.method !== 'GET') {
    res.status(405).json({ error: 'method not allowed' });
    return;
  }

  const requestedPlatform = req.query?.platform;
  if (
    requestedPlatform !== undefined &&
    (Array.isArray(requestedPlatform) || !['macos', 'windows'].includes(requestedPlatform))
  ) {
    res.status(400).json({ error: 'platform must be macos or windows' });
    return;
  }
  const platform = requestedPlatform === 'windows' ? 'windows' : 'macos';
  const suffix = platform === 'windows' ? '_windows_x86_64-setup.exe' : '_universal.dmg';

  try {
    const blob = await findBlobByPrefixSuffix('Petal_', suffix);
    if (!blob) {
      res.status(404).json({ error: 'no release published yet' });
      return;
    }
    res.setHeader('Location', blob.url);
    res.status(302).end();
  } catch (err) {
    // #282: previously a bespoke inline catch with ZERO console logging on
    // unexpected failures — routed through sendApiError so download failures
    // are now logged + Sentry-reported like every other route.
    await sendApiError(res, err, {
      operation: '/api/download GET',
      fallbackStatus: 502,
      fallbackMessage: 'download service unavailable',
    });
  }
}
