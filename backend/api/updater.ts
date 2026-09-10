// GET /api/updater -> the Tauri updater manifest (latest.json), verbatim.
// This is the endpoint the app's tauri-plugin-updater is configured to hit.
// CI produces latest.json and uploads it to Vercel Blob at the stable
// pathname "latest.json" (issue #104); this handler only serves it.
//
// #125: deliberately NOT counted by platform here. The request carries no
// platform — the client picks its own key out of the manifest — and the only
// per-request signal left is the User-Agent, which is off limits (a person's
// header, and tauri-plugin-updater's is platform-less anyway). Do not add
// UA sniffing. The clean path is `{{target}}` in the configured updater
// endpoint (tauri.release.conf.json), which is a client change only new
// builds would send; /api/download is the countable endpoint today.

import type { VercelRequest, VercelResponse } from '../lib/vercel.js';
import { findBlobByPathname, fetchBlobJson } from '../lib/blob.js';
import { applyCors } from '../lib/http.js';
import { captureApiError, errorTypeName, flushSentry } from '../lib/sentry.js';

export default async function handler(req: VercelRequest, res: VercelResponse) {
  if (applyCors(req, res)) return;
  if (req.method !== 'GET') {
    res.status(405).json({ error: 'method not allowed' });
    return;
  }
  try {
    const blob = await findBlobByPathname('latest.json');
    if (!blob) {
      // No release published yet. Return 204 No Content, which
      // tauri-plugin-updater reads as "no update available" (silent). A 404 is
      // treated as an endpoint error and logged as "update endpoint did not
      // respond with a successful status code" on every launch (issue #177).
      res.status(204).end();
      return;
    }
    const manifest = await fetchBlobJson(blob);
    res.setHeader('Content-Type', 'application/json');
    res.status(200).json(manifest);
  } catch (err) {
    // #177: this route deliberately downgrades to a silent 204 (read as "no
    // update available" by tauri-plugin-updater) rather than a 5xx — that
    // response contract is unchanged. #282 only adds server-side visibility
    // alongside the existing warn: report to Sentry (5xx-equivalent, since a
    // real failure is being masked from the caller) and flush before this
    // function returns, same as every other route.
    console.warn(
      'updater: latest.json unavailable',
      err instanceof Error ? err.message : 'unknown error'
    );
    await captureApiError(err, {
      operation: '/api/updater GET',
      route: '/api/updater GET',
      statusCode: 204,
      errorType: errorTypeName(err),
    });
    await flushSentry(2000);
    res.status(204).end();
  }
}
