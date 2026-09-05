/**
 * Platform detection for platform-gated UI (Windows vs macOS).
 *
 * UA-based on purpose: no new dependency, works in real Tauri windows
 * (WebView2 UAs contain `Windows NT`, WKWebView UAs contain `Macintosh`)
 * and in plain-browser preview. The `ua` parameter exists only for tests;
 * callers use the default `navigator.userAgent`.
 */

export type PlatformKey = 'windows' | 'macos' | 'other';

export function platformKey(
  ua: string = typeof navigator !== 'undefined' ? navigator.userAgent : ''
): PlatformKey {
  if (/Windows/i.test(ua)) return 'windows';
  if (/Macintosh|Mac OS X|Mac_PowerPC/i.test(ua) && !/iPad|iPhone|Mobile/i.test(ua)) return 'macos';
  return 'other';
}

export function isWindows(ua?: string): boolean {
  return platformKey(ua) === 'windows';
}

export function isMac(ua?: string): boolean {
  return platformKey(ua) === 'macos';
}
