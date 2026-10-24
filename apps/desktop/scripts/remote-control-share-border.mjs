// Attribution for the live gate's share-border stack assertion (#154).
//
// The assertion used to say one thing -- "share border missing or not
// on-screen for window N" -- for two states that mean opposite things:
//
//   1. the border is gone while its source window is still being shared
//      (a real, user-visible product failure: the user is sharing a window
//      with nothing telling them so), and
//   2. the border was deliberately ordered out by the border tracker because
//      its SOURCE window left the window stack (correct behaviour -- the
//      border follows its source).
//
// One intermittent release-gate failure (run 34459415604, 09:23:34.984) was
// state 2: the harness's own photon-sentinel window vanished ~470ms after its
// share started, Petal ordered the border out, and 33ms later Petal tore the
// share down because the window was gone. The message accused the border.
//
// `share_border_stack` now returns `shareState`, so the two are separable
// WITHOUT retrying the assertion -- a retry would have hidden state 1 forever.

/** Share states in which an off-screen border is the CORRECT outcome. */
const SOURCE_GONE_SHARE_STATES = new Set(['notShared', 'offScreen', 'closed']);

export function shareStateSummary(report) {
  const state = report && report.shareState;
  if (!state || typeof state !== 'object') return 'shareState=unreported';
  return [
    `share=${state.share ?? 'unknown'}`,
    `borderRegistered=${state.registered === true}`,
    `trackerHidden=${state.trackerHidden === true}`,
    `hideRequested=${state.hideRequested === true}`,
  ].join(' ');
}

/**
 * True when the reported share state says the border's SOURCE went away, so
 * an off-screen border is expected rather than broken.
 */
export function sourceWindowIsGone(report) {
  const state = report && report.shareState;
  if (!state || typeof state !== 'object') return false;
  return (
    state.trackerHidden === true ||
    state.hideRequested === true ||
    SOURCE_GONE_SHARE_STATES.has(state.share)
  );
}

/**
 * Classify one `share_border_stack` report.
 *
 * @returns {{ok: true, summary: string}
 *          |{ok: false, reason: string, message: string}}
 */
export function classifyShareBorderStackReport(report, windowId) {
  if (report == null || typeof report !== 'object') {
    return {
      ok: false,
      reason: 'no-report',
      message: `share_border_stack returned no report for window ${windowId}: ${JSON.stringify(report)}`,
    };
  }
  const state = shareStateSummary(report);
  const detail = `${state}: ${JSON.stringify(report)}`;

  if (report.border == null || report.border.stackIndex == null) {
    if (sourceWindowIsGone(report)) {
      return {
        ok: false,
        reason: 'source-window-lost',
        message:
          `share border for window ${windowId} is off-screen because its SOURCE window left the window stack, ` +
          `not because the border broke -- the shared window went away mid-assertion (#154). ${detail}`,
      };
    }
    return {
      ok: false,
      reason: 'border-missing',
      message: `share border missing or not on-screen for window ${windowId} while its share is live -- ${detail}`,
    };
  }
  if (report.source == null || report.source.stackIndex == null) {
    return {
      ok: false,
      reason: 'source-missing',
      message: `shared source window ${windowId} not found in the on-screen stack -- ${detail}`,
    };
  }
  if (report.border.stackIndex >= report.source.stackIndex) {
    return {
      ok: false,
      reason: 'border-behind-source',
      message: `share border is not stacked in front of its source window -- ${detail}`,
    };
  }
  return {
    ok: true,
    summary: `# share-border-stack window=${windowId} border=${report.border.stackIndex} source=${report.source.stackIndex} (border in front) ${state}`,
  };
}

// share_border.rs's tracker polls every 100ms and reorders a border found
// BEHIND its source on the very next tick (no debounce; #300 bounds even the
// debounced in-front case to ~400ms). A single stack read can land inside
// that tick -- run 40004629026 read border=11 source=10 at 18:51:13.790 and
// the tracker reordered at 13.804 -- so a behind-source read is re-checked
// for this long, and only a misorder that outlives it fails.
export const BORDER_TRACKER_SETTLE_MS = 500;

/**
 * Read and classify the border stack, re-reading ONLY a `border-behind-source`
 * verdict until it converges or `settleMs` passes. Every other failure
 * (border missing, source lost) is returned from the first read unchanged.
 *
 * @param {() => Promise<unknown>} readReport one `share_border_stack` call
 */
export async function awaitShareBorderStacked(
  readReport,
  windowId,
  {
    settleMs = BORDER_TRACKER_SETTLE_MS,
    pollMs = 50,
    now = () => Date.now(),
    sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
  } = {}
) {
  const started = now();
  const first = classifyShareBorderStackReport(await readReport(), windowId);
  if (first.ok || first.reason !== 'border-behind-source') return first;
  let last = first;
  while (now() - started < settleMs) {
    await sleep(pollMs);
    last = classifyShareBorderStackReport(await readReport(), windowId);
    if (last.ok) {
      const elapsedMs = now() - started;
      return {
        ...last,
        summary: `${last.summary} -- converged after ${elapsedMs}ms; first read had the border behind its source (within the ${settleMs}ms tracker settle bound)`,
      };
    }
    if (last.reason !== 'border-behind-source') return last;
  }
  return {
    ...last,
    message: `${last.message} (still behind after the ${settleMs}ms border-tracker settle bound; first read: ${first.message})`,
  };
}
