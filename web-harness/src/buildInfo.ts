import { isStrictSemVer } from './strictSemver.mjs';

export function validateReleaseVersion(value: unknown, source: string): string {
  if (!isStrictSemVer(value)) {
    throw new Error(`${source} must be a valid semantic version, got ${String(value)}`);
  }
  if (value === '0.0.0') {
    throw new Error(`${source} must not be 0.0.0`);
  }
  return value;
}

export type DesktopMetadata =
  | { status: 'missing' }
  | { status: 'present'; version: unknown }
  | { status: 'unreadable'; message: string };

/**
 * Resolve the checked-in web release mirror. Missing desktop metadata is
 * allowed only when the caller has proved this is an isolated Vercel build.
 */
export function resolveBuildVersion(
  webVersion: unknown,
  desktopMetadata: DesktopMetadata,
  options: { allowMissingDesktopMetadata?: boolean } = {}
): string {
  const web = validateReleaseVersion(webVersion, 'web-harness version');

  if (desktopMetadata.status === 'missing') {
    if (!options.allowMissingDesktopMetadata) {
      throw new Error('desktop package metadata is missing outside an isolated Vercel build');
    }
    return web;
  }

  if (desktopMetadata.status === 'unreadable') {
    throw new Error(`desktop package metadata could not be read: ${desktopMetadata.message}`);
  }

  const desktop = validateReleaseVersion(desktopMetadata.version, 'desktop version');
  if (web !== desktop) {
    throw new Error(`web-harness version ${web} does not match desktop version ${desktop}`);
  }
  return web;
}
