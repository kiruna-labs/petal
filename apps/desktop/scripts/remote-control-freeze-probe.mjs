// Presented-video freeze probe for the release e2e loopback (#378).
//
// While the photon sentinel animates continuously (SIGUSR1, 30Hz motion
// strip), the controller's browser records every requestVideoFrameCallback
// on the shared window's <video>. A gap between presented frames is then a
// real freeze the viewer saw -- an idle source cannot cause one. The edges
// count too: no frame for the first N ms of the window is an N ms freeze.
// Scored by scripts/ci/remote-control-latency-verdict.mjs (`presentedFreeze`).

export const FREEZE_PROBE_DEFAULT_GAP_MS = 500;

function round(value) {
  return Math.round(value * 10) / 10;
}

// times: rVFC callback `now` values (performance.now domain), any order.
export function summarizePresentedGaps(times, { startMs, endMs, thresholdMs = FREEZE_PROBE_DEFAULT_GAP_MS }) {
  const frames = (times ?? []).filter((t) => Number.isFinite(t) && t >= startMs && t <= endMs).sort((a, b) => a - b);
  const edges = [startMs, ...frames, endMs];
  const gaps = [];
  for (let i = 1; i < edges.length; i += 1) {
    gaps.push({ atMs: round(edges[i - 1] - startMs), gapMs: round(edges[i] - edges[i - 1]) });
  }
  const freezes = gaps.filter((gap) => gap.gapMs > thresholdMs);
  return {
    durationMs: round(endMs - startMs),
    frames: frames.length,
    fps: endMs > startMs ? round((frames.length * 1000) / (endMs - startMs)) : null,
    maxGapMs: gaps.length ? Math.max(...gaps.map((gap) => gap.gapMs)) : null,
    thresholdMs,
    freezes: freezes.length,
    frozenMs: round(freezes.reduce((sum, gap) => sum + gap.gapMs, 0)),
    // The worst few, with their offset into the window, for the artifact.
    worstGaps: [...gaps].sort((a, b) => b.gapMs - a.gapMs).slice(0, 5),
  };
}

// Browser-side: resolves with the raw rVFC times plus decode-side context
// (playback quality deltas, the harness's own stall stats when present).
export function freezeProbeExpression({ windowId, tileId = null, durationMs }) {
  return `(async () => {
    const byId = ${JSON.stringify(tileId)} ? document.getElementById(${JSON.stringify(tileId)}) : null;
    const tile = byId ?? document.querySelector('.share-tile[data-window-id="${Number(windowId)}"]');
    const video = tile?.querySelector('video');
    if (!video?.requestVideoFrameCallback) throw new Error('freeze probe: no <video> with requestVideoFrameCallback for window ${Number(windowId)}');
    const quality = () => { const q = video.getVideoPlaybackQuality?.(); return q ? { total: q.totalVideoFrames, dropped: q.droppedVideoFrames } : null; };
    const measurement = window.__petalHarness?.testPatternMeasurement;
    try { measurement?.resetStallStats?.(${Number(windowId)}); } catch {}
    const q0 = quality();
    const times = [];
    let stop = false;
    const startMs = performance.now();
    const onFrame = (now) => { times.push(now); if (!stop) video.requestVideoFrameCallback(onFrame); };
    video.requestVideoFrameCallback(onFrame);
    await new Promise((resolve) => setTimeout(resolve, ${Number(durationMs)}));
    stop = true;
    const endMs = performance.now();
    const q1 = quality();
    let stall = null;
    try { stall = measurement?.stallStats?.(${Number(windowId)}) ?? null; } catch {}
    return {
      startMs, endMs, times,
      playback: q0 && q1 ? { framesDelta: q1.total - q0.total, droppedDelta: q1.dropped - q0.dropped } : null,
      decodeStall: stall,
      videoSize: { width: video.videoWidth, height: video.videoHeight }
    };
  })()`;
}
