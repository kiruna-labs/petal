// Where a user goes when the in-app updater cannot help them (#125).
//
// `GET /api/download?platform=macos|windows` (backend/api/download.ts) 302s
// to the CURRENT published artifact for that platform. Always link that, never
// a Vercel Blob URL: the blob changes every release and a hardcoded one would
// hand stranded users a stale build -- the exact failure this recovers from.
//
// Mirrors `$lib/data/inviteLinks`'s origin pattern: the official host is the
// default so official builds need no extra env, and a self-hosted deployment
// overrides it at build time (docs/SELF_HOSTING.md).
import { platformKey, type PlatformKey } from '../platform.ts';

export const DOWNLOAD_ORIGIN =
  (import.meta.env?.VITE_PETAL_BACKEND_URL as string | undefined)?.trim().replace(/\/$/, '') ||
  'https://app.petal.live';

/** The download endpoint takes exactly `macos` or `windows`; an unrecognised
 *  platform gets the macOS artifact, matching the backend's own default. */
export function manualDownloadUrl(platform: PlatformKey = platformKey()): string {
  return `${DOWNLOAD_ORIGIN}/api/download?platform=${platform === 'windows' ? 'windows' : 'macos'}`;
}
