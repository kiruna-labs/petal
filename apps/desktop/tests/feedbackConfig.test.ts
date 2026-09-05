import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { isValidUserDispatchPublicKey } from '../src/lib/feedback/config.ts';

// config.ts's `userDispatchPublicKey()` reads `import.meta.env.VITE_*`, which
// only exists under a real Vite build -- exercised here via the pure
// validator it delegates to (same split the approved web-harness parity
// reference uses, #293) plus source-text assertions for the parts that need
// a real Vite runtime.
const configSource = readFileSync(new URL('../src/lib/feedback/config.ts', import.meta.url), 'utf8');
const mainMenuSource = readFileSync(
  new URL('../src/lib/components/MainMenu.svelte', import.meta.url),
  'utf8'
);
const gallerySource = readFileSync(
  new URL('../src/lib/components/Gallery.svelte', import.meta.url),
  'utf8'
);
const meetingChromeSource = readFileSync(
  new URL('../src/lib/components/MeetingChrome.svelte', import.meta.url),
  'utf8'
);
const meetingRouteSource = readFileSync(
  new URL('../src/routes/meeting/[room]/+page.svelte', import.meta.url),
  'utf8'
);

test('isValidUserDispatchPublicKey accepts only pk_-prefixed public keys', () => {
  assert.equal(isValidUserDispatchPublicKey('pk_live_abcd1234'), true);
  assert.equal(isValidUserDispatchPublicKey('pk_test_ABCD-1234_xyz'), true);
});

test('isValidUserDispatchPublicKey rejects secret keys, empty, and malformed values', () => {
  assert.equal(isValidUserDispatchPublicKey('sk_live_abcd1234'), false, 'must reject a secret key shape');
  assert.equal(isValidUserDispatchPublicKey(''), false);
  assert.equal(isValidUserDispatchPublicKey('   '), false);
  assert.equal(isValidUserDispatchPublicKey(null), false);
  assert.equal(isValidUserDispatchPublicKey(undefined), false);
  assert.equal(isValidUserDispatchPublicKey('pk_short'), false, 'too short after the prefix');
  assert.equal(isValidUserDispatchPublicKey('not-a-key-at-all'), false);
});

test('config.ts reads the public key from a Vite build-time env var, not a Rust option_env! bake', () => {
  // This must be a `VITE_`-prefixed var (readable in the webview by the
  // bundled SDK), NOT `PETAL_USERDISPATCH_API_KEY`/option_env! -- the
  // widget-script design that used that pattern was explicitly rejected by
  // the issue's resolution comment in favor of a Petal-owned modal + bundled
  // @userdispatch/sdk with a public key only.
  assert.match(configSource, /VITE_USERDISPATCH_PUBLIC_KEY/);
  assert.doesNotMatch(configSource, /PETAL_USERDISPATCH_API_KEY/);
});

test('feedback is compiled off entirely (no trigger, no SDK reference) when the key is absent -- static source', () => {
  // No hosted widget/loader script is EVER referenced anywhere near the
  // gate or the main-menu trigger -- the desktop implementation never loads
  // widget.js, unlike the issue's original (superseded) plan.
  assert.doesNotMatch(configSource, /widget\.js/);
  assert.doesNotMatch(configSource, /<script/);
  assert.doesNotMatch(configSource, /createElement\('script'\)/);

  // The main-menu trigger only renders inside a `feedbackEnabled` gate that
  // is derived from `isFeedbackEnabled()` -- an absent/invalid key means
  // `{#if feedbackEnabled}` never renders the button at all.
  assert.match(mainMenuSource, /import \{ isFeedbackEnabled \} from '\$lib\/feedback\/config'/);
  assert.match(mainMenuSource, /const feedbackEnabled = isFeedbackEnabled\(\);/);
  assert.match(mainMenuSource, /\{#if feedbackEnabled\}/);
});

test('#786: the meeting topbar bug-report cell sits between the layout toggle and Connection stats', () => {
  const topbarRightAt = gallerySource.indexOf('<div class="topbar-right">');
  const layoutToggleAt = gallerySource.indexOf('layout-toggle', topbarRightAt);
  const reportBugAt = gallerySource.indexOf('report-bug', topbarRightAt);
  const netBtnAt = gallerySource.indexOf('net-btn', topbarRightAt);
  assert.ok(topbarRightAt !== -1 && layoutToggleAt !== -1, 'gallery topbar-right lost its layout toggle');
  assert.ok(reportBugAt !== -1, 'the bug-report button must render inside the gallery topbar');
  assert.ok(layoutToggleAt < reportBugAt, 'the bug report belongs immediately right of the layout toggle');
  assert.ok(reportBugAt < netBtnAt, 'the bug report belongs left of the Connection stats cell');

  // It reuses the existing cell + tooltip chrome (whose wrap-not-truncate
  // rules transientTextTruncation.test.ts already pins), and it is NOT the
  // trailing cell -- so `.topbar-control-cell:last-of-type`'s tooltip
  // right-alignment keeps targeting the same cell it did before.
  const cell = gallerySource.slice(reportBugAt - 400, netBtnAt);
  assert.match(cell, /<div class="topbar-control-cell">/);
  assert.match(
    gallerySource.slice(reportBugAt, netBtnAt),
    /<span class="topbar-tooltip" aria-hidden="true">\{reportBugLabel\}<\/span>/
  );
});

test('#786: the bug-report cell exists only when the route hands down a trigger, and the route gates that on the build key', () => {
  // No `onReportBug` (every build with no UserDispatch key, plus the /dev
  // harnesses) means the cell never renders -- the same "compiled off"
  // posture as the main-menu trigger, expressed through the prop.
  assert.match(gallerySource, /onReportBug\?: \(\) => void;/);
  assert.match(gallerySource, /\{#if onReportBug\}/);

  assert.match(meetingChromeSource, /onReportBug\?: \(\) => void;/);
  assert.match(meetingChromeSource, /\{onReportBug\}/);

  assert.match(meetingRouteSource, /import \{ isFeedbackEnabled \} from '\$lib\/feedback\/config'/);
  assert.match(meetingRouteSource, /const feedbackEnabled = isFeedbackEnabled\(\);/);
  assert.match(
    meetingRouteSource,
    /onReportBug=\{feedbackEnabled \? \(\) => \(feedbackOpen = true\) : undefined\}/
  );
  // Mounted from the meeting route for the first time (#786): before this,
  // FeedbackModal was reachable only from the home MainMenu.
  assert.match(
    meetingRouteSource,
    /import FeedbackModal from '\$lib\/components\/FeedbackModal\.svelte'/
  );
  assert.match(
    meetingRouteSource,
    /\{#if feedbackEnabled && feedbackOpen\}\s*\n\s*<FeedbackModal onClose=\{\(\) => \(feedbackOpen = false\)\} \/>/
  );
});

test('#786: while sharing, the bug button explains itself instead of swallowing the click', () => {
  // feedback.rs refuses a submission mid-share; the topbar says so rather
  // than offering a control that silently does nothing.
  assert.match(gallerySource, /const reportBugBlocked = \$derived\(sharingActive\);/);
  assert.match(
    gallerySource,
    /const REPORT_BUG_BLOCKED_REASON = "Bug reports pause while you're sharing a window";/
  );
  assert.match(
    gallerySource,
    /const reportBugLabel = \$derived\(reportBugBlocked \? REPORT_BUG_BLOCKED_REASON : 'Report a bug'\);/
  );

  const buttonAt = gallerySource.indexOf('report-bug');
  const button = gallerySource.slice(buttonAt, gallerySource.indexOf('</button>', buttonAt));
  // aria-disabled (not `disabled`) keeps the control hoverable AND focusable,
  // which is what lets the tooltip state the reason at all.
  assert.match(button, /aria-disabled=\{reportBugBlocked\}/);
  assert.match(button, /aria-label=\{reportBugLabel\}/);
  assert.doesNotMatch(button, /\sdisabled=/);
  assert.match(button, /if \(!reportBugBlocked\) void onReportBug\?\.\(\);/);
});
