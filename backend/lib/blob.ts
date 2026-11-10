// Vercel Blob read helpers for private auto-update distribution (the issue tracker
// #104). CI (built separately) uploads signed+notarized artifacts to STABLE
// blob pathnames (no random suffix) after each release:
//   - latest.json                                  the combined Tauri updater manifest
//   - Petal_<version>_universal.dmg                the macOS human download
//   - Petal_<version>_universal.app.tar.gz(.sig)   the macOS updater artifact + signature
//   - Petal_<version>_windows_x86_64-setup.exe     the Windows human/updater download
//   - Petal_<version>_windows_x86_64-setup.exe.sig  the Windows updater signature
//                                                  (the updater URLs are taken from
//                                                  latest.json; download routing uses
//                                                  the DMG/NSIS suffixes)
//
// This backend only ever READS from Blob (`list()`) — it never writes. The
// store is otherwise-private but every blob has a public CDN `url`; the
// `BLOB_READ_WRITE_TOKEN` env var is only needed to call `list()` at all
// (Vercel Blob has no anonymous list API), not to fetch the returned URLs.

import { list, type ListBlobResultBlob } from '@vercel/blob';

function requireBlobToken(): string {
  const token = process.env.BLOB_READ_WRITE_TOKEN;
  if (!token) {
    throw new Error('Missing Vercel Blob env: set BLOB_READ_WRITE_TOKEN');
  }
  return token;
}

// Find the current blob at an exact stable pathname (e.g. "latest.json").
// Returns null if it doesn't exist yet (no release published).
export async function findBlobByPathname(
  pathname: string,
  opts: { signal?: AbortSignal } = {}
): Promise<ListBlobResultBlob | null> {
  const token = requireBlobToken();
  const { blobs } = await list({ prefix: pathname, token, abortSignal: opts.signal });
  return blobs.find((b) => b.pathname === pathname) ?? null;
}

// Find the current blob matching a prefix + suffix (used for the versioned
// DMG filename, e.g. prefix "Petal_" suffix "_universal.dmg"). Picks the most
// recently uploaded match if more than one somehow exists.
export async function findBlobByPrefixSuffix(
  prefix: string,
  suffix: string,
  opts: { signal?: AbortSignal } = {}
): Promise<ListBlobResultBlob | null> {
  const token = requireBlobToken();
  const { blobs } = await list({ prefix, token, abortSignal: opts.signal });
  const matches = blobs.filter((b) => b.pathname.endsWith(suffix));
  if (matches.length === 0) return null;
  matches.sort((a, b) => b.uploadedAt.getTime() - a.uploadedAt.getTime());
  return matches[0];
}

// Fetch and parse a JSON blob's contents verbatim (used for latest.json).
export async function fetchBlobJson(
  blob: ListBlobResultBlob,
  opts: { signal?: AbortSignal } = {}
): Promise<unknown> {
  const res = await fetch(blob.url, { signal: opts.signal });
  if (!res.ok) {
    throw new Error(`blob fetch failed: ${res.status} ${res.statusText}`);
  }
  return res.json();
}

// Run a Blob read under a hard deadline, so a hung or retrying read cannot run
// a function into Vercel's maxDuration (a 504 that skips the route's own error
// handling). Do not pass AbortSignal.timeout() to @vercel/blob instead: its
// retry loop only stops on an AbortError, so it retries a TimeoutError (and
// any 5xx) with backoff past the limit. This races its own timer, then aborts
// with a plain AbortError, which the SDK honours and which frees the socket.
export async function withBlobDeadline<T>(
  ms: number,
  read: (signal: AbortSignal) => Promise<T>
): Promise<T> {
  const controller = new AbortController();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const deadline = new Promise<never>((_resolve, reject) => {
    timer = setTimeout(() => {
      controller.abort();
      reject(new Error(`blob read exceeded ${ms}ms`));
    }, ms);
  });
  // Deferred so a callback that throws synchronously still rejects inside the
  // race (and the timer is still cleared) rather than escaping it.
  const pending = Promise.resolve().then(() => read(controller.signal));
  pending.catch(() => {}); // the loser of the race must not surface as unhandled
  try {
    return await Promise.race([pending, deadline]);
  } finally {
    clearTimeout(timer);
  }
}
