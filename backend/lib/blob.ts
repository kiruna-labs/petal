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
  pathname: string
): Promise<ListBlobResultBlob | null> {
  const token = requireBlobToken();
  const { blobs } = await list({ prefix: pathname, token });
  return blobs.find((b) => b.pathname === pathname) ?? null;
}

// Find the current blob matching a prefix + suffix (used for the versioned
// DMG filename, e.g. prefix "Petal_" suffix "_universal.dmg"). Picks the most
// recently uploaded match if more than one somehow exists.
export async function findBlobByPrefixSuffix(
  prefix: string,
  suffix: string
): Promise<ListBlobResultBlob | null> {
  const token = requireBlobToken();
  const { blobs } = await list({ prefix, token });
  const matches = blobs.filter((b) => b.pathname.endsWith(suffix));
  if (matches.length === 0) return null;
  matches.sort((a, b) => b.uploadedAt.getTime() - a.uploadedAt.getTime());
  return matches[0];
}

// Fetch and parse a JSON blob's contents verbatim (used for latest.json).
export async function fetchBlobJson(blob: ListBlobResultBlob): Promise<unknown> {
  const res = await fetch(blob.url);
  if (!res.ok) {
    throw new Error(`blob fetch failed: ${res.status} ${res.statusText}`);
  }
  return res.json();
}
