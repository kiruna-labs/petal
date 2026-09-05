// GET /<label>/<access-code> or /<access-code> -> invite interstitial.
// The label is cosmetic only; the normalized access code derives the internal
// room credential and is the sole room selector. Keep this in sync with
// docs/CONTRACTS.md and shared/logic/joinInput.ts.
//
// Moved here from backend/api/j.ts so join links live at meet.petal.live
// (this project's domain) alongside the browser SPA that "Join in browser"
// falls back to, rather than on the bare API host (app.petal.live).

import type { VercelRequest, VercelResponse } from '@vercel/node';
import { applyCors } from './_lib/http.js';
import { credentialForAccessCode, normalizeAccessCode } from './_lib/slug.js';

// This project's own domain — the SPA "Join in browser" fallback lives here
// too. NOTE: the bare `web-harness.vercel.app` hostname is NOT ours — it
// hosts an unrelated Vue/Supabase app that redirects `/` → `/login` (#133
// follow-up), so never point the browser-join base at it.
const DEFAULT_WEB_JOIN_BASE_URL = 'https://meet.petal.live';
const DOWNLOAD_BASE_URL = 'https://app.petal.live/api/download';

type DesktopDownloadPlatform = 'macos' | 'windows';

export function desktopDownloadPlatformForUserAgent(
  userAgent: string | string[] | undefined,
): DesktopDownloadPlatform {
  const value = Array.isArray(userAgent) ? userAgent[0] : userAgent;
  return /Windows/i.test(value ?? '') ? 'windows' : 'macos';
}

export function downloadUrlForPlatform(platform: DesktopDownloadPlatform): string {
  return `${DOWNLOAD_BASE_URL}?platform=${platform}`;
}

const ROOM_CREDENTIAL_RE = /^room-[0-9a-f]{32}$/;

function roomLabelFromCredential(credential: string): string | null {
  return ROOM_CREDENTIAL_RE.test(credential) ? 'room' : null;
}

function firstQueryValue(value: string | string[] | undefined): string | undefined {
  return Array.isArray(value) ? value[0] : value;
}

function accessCodeQueryValue(value: string | string[] | undefined): string | null | undefined {
  if (Array.isArray(value)) {
    return value.length === 1 ? value[0] : null;
  }
  return value;
}

function accessCodeFromPath(urlValue: string | undefined): string | null | undefined {
  if (!urlValue) return undefined;
  let url: URL;
  try {
    url = new URL(urlValue, 'https://meet.petal.live');
  } catch {
    return undefined;
  }
  const segments = url.pathname.split('/').filter(Boolean);
  if (segments.length === 0 || segments.length > 2 || segments[0] === 'api') return undefined;
  try {
    return decodeURIComponent(segments[segments.length - 1]!);
  } catch {
    return null;
  }
}

function labelFromPath(urlValue: string | undefined): string | undefined {
  if (!urlValue) return undefined;
  try {
    const url = new URL(urlValue, 'https://meet.petal.live');
    const segments = url.pathname.split('/').filter(Boolean);
    if (segments.length !== 2 || segments[0] === 'api') return undefined;
    return decodeURIComponent(segments[0]!);
  } catch {
    return undefined;
  }
}

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

export function nativeJoinUrlForAccessCode(accessCode: string): string {
  return `petal://join/${encodeURIComponent(accessCode)}`;
}

export function webJoinUrlForAccessCode(accessCode: string): string {
  const base = (process.env.PETAL_WEB_JOIN_URL ?? DEFAULT_WEB_JOIN_BASE_URL).trim();
  const url = new URL(base || DEFAULT_WEB_JOIN_BASE_URL);
  url.pathname = '/';
  url.searchParams.set('code', accessCode);
  return url.toString();
}

export function accessCodeFromJoinRequest(
  query: VercelRequest['query'],
  url?: string,
): string | null {
  const queryCode = accessCodeQueryValue(query.code);
  if (queryCode === null) return null;
  const pathCode = accessCodeFromPath(url);
  if (pathCode === null) return null;
  // A rewritten request has the code in the query, while a direct request may
  // only have it in the path. If both exist, require them to agree so a
  // cosmetic label or a stale rewrite cannot select another room.
  if (queryCode !== undefined && pathCode !== undefined) {
    const normalizedQueryCode = normalizeAccessCode(queryCode);
    const normalizedPathCode = normalizeAccessCode(pathCode);
    if (!normalizedQueryCode || normalizedQueryCode !== normalizedPathCode) return null;
  }
  const accessCode = queryCode ?? pathCode;
  return accessCode ? normalizeAccessCode(accessCode) : null;
}

export function credentialFromJoinQuery(
  query: VercelRequest['query'],
  url?: string,
): string | null {
  const accessCode = accessCodeFromJoinRequest(query, url);
  return accessCode ? credentialForAccessCode(accessCode) : null;
}

export function inviteInterstitialHtml(args: {
  credential: string;
  accessCode?: string;
  label?: string;
  nativeJoinUrl?: string;
  webJoinUrl?: string;
  downloadPlatform?: DesktopDownloadPlatform;
}): string {
  const credential = args.credential;
  const accessCode = normalizeAccessCode(args.accessCode ?? '') ?? credential;
  const nativeJoinUrl = args.nativeJoinUrl ?? nativeJoinUrlForAccessCode(accessCode);
  const webJoinUrl = args.webJoinUrl ?? webJoinUrlForAccessCode(accessCode);
  const label = args.label?.trim() || roomLabelFromCredential(credential) || 'this room';
  const title = `Join ${label}`;
  const downloadPlatform = args.downloadPlatform ?? 'macos';
  const alternateDownloadPlatform = downloadPlatform === 'windows' ? 'macos' : 'windows';
  const downloadLabel = downloadPlatform === 'windows' ? 'Download Petal for Windows' : 'Download Petal for macOS';
  const alternateDownloadLabel = alternateDownloadPlatform === 'windows' ? 'Download Petal for Windows' : 'Download Petal for macOS';
  const downloadNotice = downloadPlatform === 'windows'
    ? '<p class="fine">Windows downloads are currently unsigned and may show a SmartScreen warning.</p>'
    : '';

  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>${escapeHtml(title)} | Petal</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body {
    margin: 0;
    min-height: 100vh;
    display: flex;
    align-items: center;
    justify-content: center;
    background: #0e0c10;
    color: #f5f6f7;
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
    -webkit-font-smoothing: antialiased;
    padding: 24px;
  }
  main {
    width: min(100%, 480px);
    text-align: center;
  }
  .brand {
    margin: 0 0 28px;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 13px;
    font-weight: 700;
    color: rgba(245, 246, 247, 0.58);
    text-transform: lowercase;
  }
  .brand-mark {
    display: block;
    flex-shrink: 0;
    color: rgba(245, 246, 247, 0.58);
  }
  h1 {
    margin: 0 0 12px;
    font-size: clamp(28px, 8vw, 42px);
    line-height: 1.06;
    font-weight: 760;
    text-wrap: balance;
  }
  .copy {
    max-width: 36rem;
    margin: 0 auto 28px;
    font-size: 15px;
    line-height: 1.55;
    color: rgba(245, 246, 247, 0.72);
    text-wrap: pretty;
  }
  .actions {
    display: grid;
    grid-template-columns: 1fr;
    gap: 10px;
    margin: 0 auto;
  }
  a.button {
    display: inline-flex;
    min-height: 46px;
    align-items: center;
    justify-content: center;
    padding: 12px 18px;
    border-radius: 10px;
    color: #f5f6f7;
    background: rgba(245, 246, 247, 0.1);
    box-shadow: inset 0 0 0 1px rgba(245, 246, 247, 0.12);
    font-size: 14px;
    font-weight: 720;
    text-decoration: none;
    transition-property: opacity, transform, background;
    transition-duration: 0.15s;
    transition-timing-function: cubic-bezier(0.2, 0, 0, 1);
  }
  a.button:hover { opacity: 0.88; }
  a.button:active { transform: scale(0.96); }
  a.primary {
    background: #f5f6f7;
    color: #0e0c10;
    box-shadow: none;
  }
  a.secondary {
    background: rgba(245, 246, 247, 0.075);
  }
  .fine {
    margin: 22px 0 0;
    font-size: 12px;
    line-height: 1.45;
    color: rgba(245, 246, 247, 0.46);
  }
  code {
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    font-size: 11px;
    word-break: break-all;
    color: rgba(245, 246, 247, 0.62);
  }
  .code-copy {
    display: inline-block;
    margin: 2px -6px -2px;
    padding: 2px 6px;
    background: none;
    border: none;
    border-radius: 5px;
    font: inherit;
    cursor: pointer;
    transition: background-color 0.15s ease;
  }
  .code-copy:hover,
  .code-copy:focus-visible {
    background: rgba(245, 246, 247, 0.1);
    outline: none;
  }
  .code-copy:active {
    transform: scale(0.97);
  }
  @media (min-width: 560px) {
    .actions { grid-template-columns: 1fr 1fr; }
    a.primary { grid-column: 1 / -1; }
  }
</style>
</head>
<body>
<main>
  <p class="brand">
    <svg class="brand-mark" width="16" height="16" viewBox="0 0 936 961" fill="none" xmlns="http://www.w3.org/2000/svg" aria-hidden="true" focusable="false">
      <path fill="currentColor" fill-rule="evenodd" clip-rule="evenodd" d="M 467 1.4 C 465.1 2.1, 441.5 19.7, 430.3 28.7 C 424.4 33.4, 409.9 45.6, 406.3 48.9 C 363.9 87.3, 331.1 127, 306 170.3 C 294.9 189.4, 281.4 218, 274.5 236.6 C 266.1 259.8, 259.7 285, 260.7 291.9 C 261.4 296.8, 264.3 300.1, 271.8 304.2 C 290.1 314.4, 318.7 332.6, 339 346.9 C 362.6 363.6, 382.7 380.7, 400.1 399 C 419.8 419.8, 428.4 430.8, 450.9 463.5 C 461.5 478.9, 464.9 482.7, 468.7 483.3 C 470.1 483.6, 470.9 483.4, 472.5 482.3 C 475 480.6, 477 478, 489.5 459.5 C 502.4 440.5, 505 436.8, 512.6 427.4 C 528.3 407.9, 548.3 387, 566.3 371.4 C 584 356.1, 602.8 342.8, 640.5 319.3 C 667.2 302.6, 668.4 301.7, 671.2 296.8 C 676.8 287.3, 674.3 269.5, 662.7 236.5 C 656.9 220.2, 645.3 194.9, 635.8 177.8 C 599.7 113.2, 545.7 54.4, 480.4 8.9 C 468.7 0.7, 468.7 0.7, 467 1.4 M 458.7 85.8 C 406.9 130.4, 367.7 180.6, 344.7 232.1 C 338.2 246.6, 331.5 264, 331.5 266.3 C 331.5 267.7, 332.7 268.7, 341.3 273.8 C 366.3 288.7, 395.2 310, 421.2 333 C 433.4 343.7, 454.8 365.7, 466.6 379.7 C 467.7 381, 468.1 381.2, 469.1 380.7 C 469.8 380.5, 472.1 378.2, 474.1 375.8 C 503 341.1, 548.8 302.9, 596.7 273.4 C 601.5 270.5, 605.3 267.8, 605.5 267.2 C 606.4 264.7, 595.2 236.2, 586.2 218.4 C 580.2 206.4, 570.2 189.4, 562.8 178.5 C 546.3 154.3, 532.6 138, 508.3 113.7 C 487.4 92.9, 471.2 78.5, 468.6 78.5 C 467.6 78.5, 465.2 80.3, 458.7 85.8 M 11.9 414.5 C 7.8 414.7, 6.3 415, 5.7 415.6 C 5 416.3, 4.6 419.3, 3.5 431.6 C 1.3 455.3, 0.8 465.3, 1.1 487.6 C 1.5 514.9, 3.3 534.2, 8 562.8 C 13 592.5, 19 616.3, 27.8 641.4 C 36.2 665.4, 42.5 680.5, 52.3 700.3 C 63.6 723.3, 70 734.3, 83.5 754.8 C 112.9 798.9, 144.9 832.8, 188.9 866.3 C 232.9 899.8, 283.1 925.5, 335.9 941.5 C 368 951.3, 396.7 956.7, 433.3 959.9 C 446 961, 489.4 961.1, 501.5 960.1 C 549.4 956, 587.7 947.6, 629.9 931.9 C 646.2 925.9, 658.2 920.6, 676.5 911.4 C 696.5 901.4, 705.4 896.3, 722.9 884.7 C 785.1 843.4, 835.7 790, 870.9 728.5 C 876.9 718, 888.5 694.7, 894.2 681.8 C 915.6 633.2, 928 585.3, 934.3 525.8 C 935.7 513.1, 935.7 512, 935.7 483.3 C 935.8 453.1, 935.7 451.1, 933.5 430.3 C 932.2 418.9, 931.8 416.6, 930.8 415.6 C 929 413.8, 907.1 413.7, 886 415.3 C 848.7 418.2, 813.1 424.9, 778.8 435.7 C 753.5 443.6, 740.4 448.4, 724.6 455.5 C 679.5 476, 639 502.2, 602.9 534.3 C 573.2 560.8, 548.2 589.5, 526.5 622 C 513.1 642.1, 507.3 652.1, 497.1 672.5 C 487.3 691.9, 480.2 708.5, 473 728.3 C 469.8 737.1, 469.1 738.5, 468.1 737.4 C 467.9 737, 466.5 733.5, 465 729.5 C 457.5 708.8, 452.4 696.7, 441.7 675.3 C 427.8 647.1, 416 627.7, 398.1 603.8 C 383.2 583.9, 371.9 570.8, 355 554 C 325 523.9, 289.2 496.4, 255.5 477.2 C 237.2 466.7, 205.6 451.8, 188.9 445.5 C 173.3 439.8, 148.2 432.1, 131.3 427.8 C 93 418.3, 43.7 412.7, 11.9 414.5 M 64.6 480.6 C 63.8 481.4, 63.8 494, 64.5 506.1 C 67.2 552.6, 77.3 598.1, 94 640.1 C 104.8 667.1, 116.3 689.8, 131.2 713.2 C 156.7 753.6, 192.1 791.3, 231.5 820 C 267.2 846, 304.8 865.1, 347.8 879 C 368.6 885.7, 399 892.3, 423 895.2 C 427.9 895.8, 429.3 895.8, 429.9 895.4 C 431.3 894.2, 429.9 864.3, 427.5 844.5 C 418.7 771.8, 390.5 699.1, 349.2 642.4 C 322.4 605.6, 287.2 572.2, 247.5 545.6 C 230.2 534.1, 201.1 518.3, 183.5 510.9 C 149.7 496.5, 107.1 484.9, 73.5 480.8 C 66 479.8, 65.4 479.8, 64.6 480.6 M 868 480.6 C 857.2 482, 852.3 482.8, 843.9 484.3 C 790.1 494.2, 743.6 512.1, 697.8 540.7 C 672.9 556.2, 649.7 574.7, 628.5 596 C 599.5 624.9, 579.9 651.1, 559.7 687.8 C 541.1 721.6, 525.2 764.9, 516.7 805.3 C 510.7 833.8, 507.7 857.8, 506.7 885.8 C 506.4 893.6, 506.5 894.9, 507.2 895.4 C 509.1 897, 549.2 890.1, 570.8 884.5 C 593.8 878.5, 613.6 871.3, 639.8 859.2 C 657.8 850.9, 680 838.3, 696.5 826.9 C 719.2 811.1, 736.3 796.8, 755 777.7 C 778 754.2, 794.7 732.8, 810.6 706.5 C 850.4 640.6, 871.6 568.8, 873.4 494.6 C 873.6 482.1, 873.6 481.3, 872.7 480.7 C 872.2 480.3, 871.7 480, 871.5 480.1 C 871.4 480.1, 869.8 480.3, 868 480.6" />
    </svg>
    <span>Petal</span>
  </p>
  <h1>${escapeHtml(title)}</h1>
  <p class="copy">Opening the desktop app. If nothing happens, download Petal for Windows or macOS, or join from this browser.</p>
  <div class="actions">
    <a class="button primary" href="${escapeHtml(nativeJoinUrl)}">Open Petal</a>
    <a class="button" href="${escapeHtml(downloadUrlForPlatform(downloadPlatform))}">${downloadLabel}</a>
    <a class="button" href="${escapeHtml(downloadUrlForPlatform(alternateDownloadPlatform))}">${alternateDownloadLabel}</a>
    <a class="button secondary" href="${escapeHtml(webJoinUrl)}" rel="noreferrer">Join in browser</a>
  </div>
  ${downloadNotice}
  <p class="fine">Meeting code<br />
    <button type="button" class="code-copy" id="code-copy" aria-label="Copy invite link">
      <code id="code-text">${escapeHtml(accessCode)}</code>
    </button>
  </p>
</main>
<script>
  window.setTimeout(function () {
    window.location.href = ${JSON.stringify(nativeJoinUrl)};
  }, 150);
</script>
<script>
  (function () {
    var btn = document.getElementById('code-copy');
    var codeEl = document.getElementById('code-text');
    if (!btn || !codeEl) return;
    var original = codeEl.textContent;
    var resetTimer = null;
    btn.addEventListener('click', function () {
      var link = window.location.href;
      function settle(ok) {
        codeEl.textContent = ok ? 'Copied!' : original;
        if (resetTimer) window.clearTimeout(resetTimer);
        resetTimer = window.setTimeout(function () {
          codeEl.textContent = original;
        }, 1400);
      }
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(link).then(function () {
          settle(true);
        }, function () {
          settle(false);
        });
      } else {
        settle(false);
      }
    });
  })();
</script>
</body>
</html>
`;
}

export default async function handler(req: VercelRequest, res: VercelResponse) {
  if (applyCors(req, res)) return;
  if (req.method !== 'GET') {
    res.status(405).json({ error: 'method not allowed' });
    return;
  }

  const accessCode = accessCodeFromJoinRequest(req.query, req.url);
  const credential = accessCode ? credentialForAccessCode(accessCode) : null;
  if (!credential) {
    res.status(400).json({ error: 'invalid invite credential' });
    return;
  }

  const label = firstQueryValue(req.query.label) ?? labelFromPath(req.url);
  const downloadPlatform = desktopDownloadPlatformForUserAgent(req.headers['user-agent']);
  res.setHeader('Content-Type', 'text/html; charset=utf-8');
  res.status(200).send(inviteInterstitialHtml({
    credential,
    accessCode: accessCode ?? undefined,
    label,
    downloadPlatform,
  }));
}
