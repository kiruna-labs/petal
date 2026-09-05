// Share-readiness probe + acceptance predicates for the remote-control suite.
//
// Why this is a module rather than an inline closure: `waitForLiveTile` built a
// rich state (found / tileId / readyState / videoWidth) and then threw it away
// with `.then(state => state?.live ? state : null)`, so every timeout reported
// `last=null`. That is why five consecutive share-ready failures on 2026-08-10
// produced no information whatsoever. `{found:false}`,
// `{found:true, readyState:0}` and `{readyState:4, videoWidth:0}` are three
// different bugs with three different next steps.

// The CDP probe. One expression, shared by every readiness mode, so a relaxed
// mode can never be accused of measuring something different -- only the
// acceptance predicate changes.
export function liveTileProbeExpression(windowId) {
  return `(() => {
          const api = window.__petalHarness?.remoteControl;
          const target = api?.targets().find((candidate) => candidate.windowId === ${windowId});
          const tile = target?.tileId ? document.getElementById(target.tileId) : null;
          const video = tile?.querySelector('video') ?? null;
          return {
            found: !!target,
            tileId: target?.tileId ?? null,
            readyState: video?.readyState ?? -1,
            videoWidth: video?.videoWidth ?? 0,
            live: !!target && !!video && video.readyState === 4 && video.videoWidth > 0
          };
        })()`;
}

// Full gate: a decoded, sized frame is actually on screen in the browser.
export function tileIsLive(state) {
  return !!state && state.found === true && state.readyState === 4 && state.videoWidth > 0;
}

export function describeTileState(state) {
  if (!state) return 'no probe result: the CDP evaluate never returned a state';
  if (!state.found) {
    return 'no remote-control target for this window: the publication never reached the browser';
  }
  if (!state.tileId) return 'target present but it has no tile id';
  if (state.readyState < 0) return 'tile present but it contains no <video> element';
  if (state.readyState !== 4) {
    return `video present but readyState=${state.readyState}: media data never arrived`;
  }
  if (!(state.videoWidth > 0)) {
    return 'video ready but videoWidth=0: no decoded frame was ever sized';
  }
  return 'live';
}

// The one line a timeout must carry. Kept here, not inline, so it is testable
// without a live CDP session.
export function tileFailureDetail(lastState) {
  return `lastTileState=${JSON.stringify(lastState ?? null)} diagnosis=${describeTileState(lastState)}`;
}

// ---------------------------------------------------------------------------
// --input-only (plan 6c): the same probe, a relaxed acceptance predicate.
//
// SCOPE LIMIT, and it must be stated wherever this is used. --input-only
// relaxes the SHARE-READINESS predicate ONLY. It does not change `start_share`,
// which still blocks on a first captured frame. TWO distinct failure shapes have
// been observed in the logs, and this flag rescues exactly one of them.
export const INPUT_ONLY_SCOPE_LINES = Object.freeze([
  'INPUT-ONLY -- video path NOT verified.',
  '--input-only relaxes the share-readiness predicate only. It does not change start_share,',
  'which still blocks on a first captured frame. Two failure shapes have been observed:',
  '  (a) share returns and the tile never goes live -- --input-only RESCUES this;',
  '  (b) start_share itself never returns because the source window emits only empty SCK',
  '      samples (status=Idle, dirty_rects=0) -- --input-only does NOT rescue this and will',
  '      hang in the same place.',
  'Evidence: petal-dev-rc3.log is shape (a); petal-e2e-final.log is shape (b).',
  'It proves nothing about pixels reaching a viewer, press-to-photon latency, or encode/decode.',
]);

// Accept as soon as the publication is present as a controllable target. No
// video element, readyState or frame size is consulted.
export function tileIsInputReady(state) {
  return !!state && state.found === true && !!state.tileId;
}

export function shareReadinessMode(inputOnly) {
  return inputOnly ? 'target-present' : 'live-tile';
}

export function shareReadyPredicate(inputOnly) {
  return inputOnly ? tileIsInputReady : tileIsLive;
}

// THE PASS BAR for --input-only, fixed here so a later reader cannot quietly
// lower it. These are exactly the cases whose oracle is the sentinel's own
// foreign-process NSEvent ledger -- the only oracle that actually evidences
// "input landed in another process's event stream", which is the whole #779
// claim. Case 23 (Retina/secondary-display mapping) is a sentinel case but is
// deliberately EXCLUDED: it needs a second display, so it legitimately skips on
// a one-display machine. It may skip; it may never fail.
export const INPUT_ONLY_PASS_BAR_CASE_IDS = Object.freeze([5, 8, 15, 16, 21, 25, 26, 28, 29, 30]);
export const INPUT_ONLY_PASS_BAR_EXCLUDED = Object.freeze([
  { caseId: 23, reason: 'needs a second display' },
]);

export function inputOnlyPassBarVerdict(results) {
  const byId = new Map((results ?? []).map((result) => [result.caseId, result]));
  const missing = INPUT_ONLY_PASS_BAR_CASE_IDS.filter((caseId) => byId.get(caseId)?.status !== 'pass');
  const excludedFailures = INPUT_ONLY_PASS_BAR_EXCLUDED.filter(
    (excluded) => byId.get(excluded.caseId)?.status === 'fail'
  ).map((excluded) => excluded.caseId);
  return {
    required: [...INPUT_ONLY_PASS_BAR_CASE_IDS],
    passed: INPUT_ONLY_PASS_BAR_CASE_IDS.length - missing.length,
    missing,
    excludedFailures,
    met: missing.length === 0 && excludedFailures.length === 0,
  };
}
