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
import { findBlobByPathname, fetchBlobJson, withBlobDeadline } from '../lib/blob.js';
import { applyCors } from '../lib/http.js';
import { captureApiError, errorTypeName, flushSentry } from '../lib/sentry.js';

// The whole Blob read (list + manifest fetch) must finish well inside the
// function's 10s maxDuration (vercel.json), leaving room for the Sentry flush.
// A Blob call that hangs never throws, so without a deadline Vercel kills the
// function with a 504 and the fallback below never runs. Do not pass
// AbortSignal.timeout() to @vercel/blob; see withBlobDeadline in lib/blob.ts.
export const BLOB_READ_DEADLINE_MS = 4000;
let blobReadDeadlineMs = BLOB_READ_DEADLINE_MS;

// Edge-cache a good manifest briefly so most checks never touch Blob. Keep
// stale-while-revalidate short: a pulled release must stop being served soon.
export const MANIFEST_CACHE_CONTROL = 'public, max-age=0, s-maxage=60, stale-while-revalidate=120';

// Last manifest this warm instance served. On a slow or failed Blob read it is
// served instead of a 204, so a flaky store hides no update from anyone. It is
// signed release metadata, so a stale copy is safe: at worst it names an older
// version, which the client treats as "no update".
let lastGoodManifest: unknown = null;

/** Test hooks: forget the warm-instance manifest; shorten the deadline. */
export function resetUpdaterStateForTests(deadlineMs = BLOB_READ_DEADLINE_MS): void {
  lastGoodManifest = null;
  blobReadDeadlineMs = deadlineMs;
}

type ReadResult = { kind: 'absent' } | { kind: 'manifest'; manifest: unknown };

function readManifestWithDeadline(): Promise<ReadResult> {
  return withBlobDeadline(blobReadDeadlineMs, async (signal): Promise<ReadResult> => {
    const blob = await findBlobByPathname('latest.json', { signal });
    if (!blob) return { kind: 'absent' };
    return { kind: 'manifest', manifest: await fetchBlobJson(blob, { signal }) };
  });
}

export default async function handler(req: VercelRequest, res: VercelResponse) {
  if (applyCors(req, res)) return;
  if (req.method !== 'GET') {
    res.status(405).json({ error: 'method not allowed' });
    return;
  }
  // The cached 200 varies by Origin even when this request had none, so a
  // no-Origin copy is never served to a browser caller.
  res.setHeader('Vary', 'Origin');
  try {
    const result = await readManifestWithDeadline();
    if (result.kind === 'absent') {
      // No release published yet. Return 204 No Content, which
      // tauri-plugin-updater reads as "no update available" (silent). A 404 is
      // treated as an endpoint error and logged as "update endpoint did not
      // respond with a successful status code" on every launch (issue #177).
      res.status(204).end();
      return;
    }
    const manifest = result.manifest;
    lastGoodManifest = manifest;
    res.setHeader('Content-Type', 'application/json');
    res.setHeader('Cache-Control', MANIFEST_CACHE_CONTROL);
    res.status(200).json(manifest);
  } catch (err) {
    // #177: never a 5xx here. Serve the last good manifest if this instance
    // has one, otherwise a silent 204 ("no update available"). #282: report
    // the masked failure to Sentry and flush before returning.
    console.warn(
      'updater: latest.json unavailable',
      err instanceof Error ? err.message : 'unknown error'
    );
    await captureApiError(err, {
      operation: '/api/updater GET',
      route: '/api/updater GET',
      statusCode: lastGoodManifest ? 200 : 204,
      errorType: errorTypeName(err),
    });
    await flushSentry(2000);
    if (lastGoodManifest) {
      res.setHeader('Content-Type', 'application/json');
      res.setHeader('Cache-Control', 'no-store');
      res.status(200).json(lastGoodManifest);
      return;
    }
    res.status(204).end();
  }
}
