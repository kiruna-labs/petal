import type { CockpitScenarioResult, CockpitStepResult, HarnessContext } from './context.ts';
import { meetingCredentialFromInviteInput } from '@petal/shared/logic/meetingCode';
import { COCKPIT_TOPIC, type CockpitReportMessage } from './trackNames.ts';

// ---------------------------------------------------------------------------
// Test-cockpit Phase 0 walking skeleton (#254): SHARE-W2N-Q end-to-end,
// nothing else. `?auto=<scenarioId>` drives an unattended step list with NO
// CDP required: join (via `?code=`) -> self-check the local test-pattern
// render loop is actually advancing -> sharePattern -> report each step over
// the `petal.cockpit` data topic. `__petalHarness.cockpitAutoScenario.join()`/
// `.sharePattern()`
// are also exposed as plain callables for a CDP-driven dev wrapper to call
// directly instead of relying on the URL-param flow.
//
// The self-check runs FIRST, before touching the network: a headless
// Chrome whose rAF loop is throttled/frozen (e.g. backgrounded despite the
// anti-throttling launch flags) must classify as INFRA-FAIL, never as a
// false Petal regression -- this is the INFRA-FAIL vs TEST-FAIL pattern
// every later cockpit scenario reuses.
// ---------------------------------------------------------------------------

const SELF_CHECK_WINDOW_MS = 1000;
const SELF_CHECK_MIN_FRAME_DELTA = 25;
const REMOTE_SHARE_WAIT_MS = 10_000;
const MULTI_PEER_WAIT_MS = 15_000;
// #819 RC-N2W: the native controller has to render this peer's share, request
// control and drive five gestures before the ledger can say anything, so this
// window is generous by design. Timing out is reported as INFRA-FAIL -- "no
// controller showed up" is not evidence that delivery is broken.
const RC_N2W_CONTROL_WAIT_MS = 90_000;
const SOAK_DEFAULT_DURATION_MS = 10 * 60 * 1000;
const SOAK_DEFAULT_HEARTBEAT_MS = 5_000;
const encoder = new TextEncoder();

// #617: the SOAK heartbeat loop's cadence is injectable so tests can drive it
// deterministically instead of asserting an exact heartbeat count derived
// from a real wall-clock `durationMs / heartbeatMs` quotient (flaky under
// any scheduling jitter -- a real `setTimeout` has no guaranteed precision).
// Production always uses `realCockpitClock`; tests substitute a synchronous
// fake (see `tests/cockpit.test.ts`'s `createFakeCockpitClock`) so the same
// exact-count assertions the loop guarantees stay meaningful without relying
// on real timers.
export interface CockpitClock {
  now: () => number;
  sleep: (ms: number) => Promise<void>;
}

const realCockpitClock: CockpitClock = {
  now: () => Date.now(),
  sleep: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
};

class CockpitInfraError extends Error {}

type ReportFields = Partial<
  Pick<
    CockpitReportMessage,
    | 'fps'
    | 'width'
    | 'height'
    | 'cameraPublished'
    | 'cameraDisappeared'
    | 'audioPublished'
    | 'strokeDelivered'
    | 'telepointerMoved'
    | 'trackName'
    | 'windowId'
    | 'participantCount'
    | 'remoteParticipantCount'
    | 'remoteAudioAudible'
    | 'remoteAudioRms'
    | 'remoteAudioEnergyDelta'
    | 'remoteAudioDurationDelta'
    | 'remoteAudioPublisher'
    | 'remoteCameraVisible'
    | 'remoteCameraFps'
    | 'remoteCameraWidth'
    | 'remoteCameraHeight'
    | 'remoteCameraFramesDecodedDelta'
    | 'remoteCameraNonBlackRatio'
    | 'remoteCameraInterFrameDiff'
    | 'remoteCameraPublisher'
    | 'classification'
    | 'controlGranted'
    | 'receivedControlKinds'
    | 'receivedControlCount'
  >
> & {
  heartbeatCount?: number;
  heartbeatOk?: boolean;
  stallWatchOk?: boolean;
  // MULTI-3 evidence intentionally stays browser-observed. It carries a
  // typed opaque roster fingerprint, never the raw identities that created it.
  // Native owns later clock, compositor, keyframe, and transport authority.
  rosterFingerprint?: string;
  rosterFingerprintAlgorithm?: 'sha-256';
  rosterIncludesReporter?: boolean;
  rosterUnique?: boolean;
};

type MultiPeerRoster = Required<
  Pick<
    ReportFields,
    | 'participantCount'
    | 'remoteParticipantCount'
    | 'rosterFingerprint'
    | 'rosterFingerprintAlgorithm'
    | 'rosterIncludesReporter'
    | 'rosterUnique'
  >
>;

export function setupCockpit(
  ctx: HarnessContext,
  getPatternFrameCount: () => number,
  soakClockOverride?: CockpitClock
) {
  const { state, hook, cb } = ctx;
  const soakClock: CockpitClock = soakClockOverride ?? realCockpitClock;

  function reporterId(): string {
    return state.room?.localParticipant.identity ?? 'unknown';
  }

  function publishReport(message: CockpitReportMessage): Promise<void> {
    if (!state.room) return Promise.resolve();
    return state.room.localParticipant
      .publishData(encoder.encode(JSON.stringify(message)), { topic: COCKPIT_TOPIC, reliable: true })
      .catch((err) => {
        console.debug(`cockpit report publish failed: ${(err as Error).message ?? err}`);
      });
  }

  async function reportStep(
    scenarioId: string,
    step: string,
    ok: boolean,
    detail: string,
    fields: ReportFields = {}
  ): Promise<void> {
    await publishReport({
      v: 1,
      reporterId: reporterId(),
      scenarioId,
      step,
      ok,
      detail,
      sentAtMs: Date.now(),
      ...fields,
    });
  }

  async function selfCheckPatternAdvancing(): Promise<CockpitStepResult> {
    const before = getPatternFrameCount();
    await new Promise((resolve) => setTimeout(resolve, SELF_CHECK_WINDOW_MS));
    const after = getPatternFrameCount();
    const delta = after - before;
    return {
      step: 'self-check',
      ok: delta >= SELF_CHECK_MIN_FRAME_DELTA,
      detail: `local test-pattern frame counter advanced by ${delta} in ${SELF_CHECK_WINDOW_MS}ms (need >= ${SELF_CHECK_MIN_FRAME_DELTA})`,
    };
  }

  async function join(code: string): Promise<void> {
    // `code` here is a bare short access code (e.g. from `?code=`), same as
    // every other join entry point -- it must be resolved to the internal
    // `room-<32hex>` credential FIRST, exactly like the interactive
    // join-field/`autoJoinFromUrl` paths do via `meetingCredentialFromInviteInput`/
    // `parseJoinInput`. `connectToMeeting`'s token request sends its `room`
    // argument to the backend verbatim and the backend only accepts the full
    // credential form -- passing the bare access code straight through fails
    // token minting with "room credential is required" (#254 regression,
    // caught by the live SHARE-W2N-Q proof).
    const credential = meetingCredentialFromInviteInput(code);
    if (!credential) {
      throw new Error(`join: could not resolve a room credential from code "${code}"`);
    }
    await cb.connectToMeeting(credential, cb.resolveIdentity());
    // `connectToMeeting` swallows its own failures (shows a UI error toast
    // and returns normally, never rethrows -- see connection.ts's fetchToken
    // catch block), so a bare `await` here would silently look like success
    // even when the token request or room connect failed. Verify the
    // postcondition explicitly instead of trusting a clean return.
    if (!state.room) {
      throw new Error('join: connectToMeeting returned without an active room (token request or connect likely failed -- check the session log)');
    }
  }

  async function sharePattern(): Promise<void> {
    await cb.startTestPatternShare();
  }

  async function waitForRemoteShareVideo(): Promise<HTMLVideoElement> {
    const deadline = Date.now() + REMOTE_SHARE_WAIT_MS;
    while (Date.now() < deadline) {
      const videos = Array.from(document.querySelectorAll<HTMLVideoElement>('.share-tile video'));
      const video = videos.find((candidate) => candidate.videoWidth > 0 && candidate.videoHeight > 0);
      if (video) return video;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    throw new Error(`no remote share video became visible within ${REMOTE_SHARE_WAIT_MS}ms`);
  }

  async function rosterFingerprint(participantIds: string[]): Promise<string> {
    const subtle = globalThis.crypto?.subtle;
    if (!subtle) {
      throw new CockpitInfraError('SubtleCrypto is unavailable; cannot produce privacy-preserving MULTI-3 roster correlation evidence');
    }
    const digest = await subtle.digest('SHA-256', encoder.encode(JSON.stringify(participantIds)));
    return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, '0')).join('');
  }

  async function observedRoster(): Promise<MultiPeerRoster> {
    const room = state.room;
    const reporter = room?.localParticipant.identity?.trim() ?? '';
    const remoteParticipantIds = [...(room?.remoteParticipants.keys() ?? [])]
      .map((identity) => identity.trim())
      .filter(Boolean)
      .sort();
    const participantIds = reporter ? [reporter, ...remoteParticipantIds].sort() : remoteParticipantIds;
    const rosterUnique = new Set(participantIds).size === participantIds.length;
    return {
      participantCount: room ? 1 + remoteParticipantIds.length : 0,
      remoteParticipantCount: remoteParticipantIds.length,
      rosterFingerprint: await rosterFingerprint(participantIds),
      rosterFingerprintAlgorithm: 'sha-256',
      rosterIncludesReporter: reporter.length > 0 && participantIds.includes(reporter),
      rosterUnique,
    };
  }

  async function waitForParticipantCount(minParticipants: number): Promise<MultiPeerRoster> {
    const waitMs = cockpitParamMs(['multiPeerWaitMs'], MULTI_PEER_WAIT_MS);
    const deadline = Date.now() + waitMs;
    while (Date.now() < deadline) {
      const roster = await observedRoster();
      // A count alone can falsely look like MULTI-3 success if the local
      // identity is absent or the roster has been corrupted. Keep the proof
      // limited to browser-observed membership, but make it attributable.
      if (
        roster.participantCount >= minParticipants &&
        roster.remoteParticipantCount >= minParticipants - 1 &&
        roster.rosterIncludesReporter &&
        roster.rosterUnique
      ) {
        return roster;
      }
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    const roster = await observedRoster();
    throw new CockpitInfraError(
      `MULTI-3 expected an attributable unique roster of at least ${minParticipants} participants but saw ${roster.participantCount} (${roster.remoteParticipantCount} remote); reporterIncluded=${roster.rosterIncludesReporter}; unique=${roster.rosterUnique}; fingerprint=${roster.rosterFingerprint} within ${waitMs}ms`
    );
  }

  async function measureVideoFrames(video: HTMLVideoElement): Promise<{ fps: number; width: number; height: number }> {
    const requestVideoFrameCallback = video.requestVideoFrameCallback?.bind(video);
    if (!requestVideoFrameCallback) {
      throw new CockpitInfraError('requestVideoFrameCallback is unavailable; cannot measure delivered share fps');
    }

    let frames = 0;
    const startedAt = performance.now();
    let sampling = true;
    const onFrame = () => {
      if (!sampling) return;
      frames += 1;
      requestVideoFrameCallback(onFrame);
    };
    requestVideoFrameCallback(onFrame);
    // Allow a device-pixel viewer-demand request to reach LiveKit and a new
    // keyframe from the selected simulcast layer to arrive before recording
    // the decoded dimensions. Keep the wall-clock deadline independent of
    // frame delivery: the self-capture source can legitimately be static.
    await new Promise((resolve) => setTimeout(resolve, 4000));
    sampling = false;
    const elapsedSeconds = Math.max((performance.now() - startedAt) / 1000, 0.001);
    // A viewer-demand request can switch the subscribed simulcast layer while
    // this sample is running. Report the dimensions of the final decoded
    // frame, not the lower bootstrap layer that happened to arrive first.
    return { fps: frames / elapsedSeconds, width: video.videoWidth, height: video.videoHeight };
  }

  function cockpitParamMs(names: string[], fallback: number): number {
    const search = typeof location === 'undefined' ? '' : location.search;
    const params = new URLSearchParams(search);
    for (const name of names) {
      const raw = params.get(name);
      if (!raw) continue;
      const parsed = Number(raw);
      if (Number.isFinite(parsed) && parsed > 0) return parsed;
    }
    return fallback;
  }

  function soakConfig(): { durationMs: number; heartbeatMs: number } {
    return {
      durationMs: cockpitParamMs(['soakMs', 'soakDurationMs'], SOAK_DEFAULT_DURATION_MS),
      heartbeatMs: cockpitParamMs(['soakHeartbeatMs', 'heartbeatMs'], SOAK_DEFAULT_HEARTBEAT_MS),
    };
  }

  async function runSoakHeartbeatWatch(
    scenarioId: string,
    record: (step: CockpitStepResult & { fields?: ReportFields }) => Promise<void>
  ): Promise<CockpitStepResult & { fields?: ReportFields }> {
    const { durationMs, heartbeatMs } = soakConfig();
    const startedAt = soakClock.now();
    const deadline = startedAt + durationMs;
    let heartbeatCount = 0;

    await record({
      step: 'soak-start',
      ok: true,
      detail: `starting bounded SOAK heartbeat/stall-watch scaffold for ${durationMs}ms (heartbeat every ${heartbeatMs}ms)`,
    });

    while (soakClock.now() < deadline) {
      heartbeatCount += 1;
      const elapsedMs = soakClock.now() - startedAt;
      await record({
        step: 'soak-heartbeat',
        ok: true,
        detail: `SOAK heartbeat ${heartbeatCount}; elapsed=${elapsedMs}ms duration=${durationMs}ms (web harness publishes liveness, native side owns stall verdicts)`,
      });
      const remainingMs = deadline - soakClock.now();
      if (remainingMs <= 0) break;
      await soakClock.sleep(Math.min(heartbeatMs, remainingMs));
    }

    return {
      step: 'soak-complete',
      ok: true,
      detail: `${scenarioId} completed bounded heartbeat scaffold with ${heartbeatCount} heartbeat(s) over ${soakClock.now() - startedAt}ms`,
      fields: { heartbeatCount, heartbeatOk: heartbeatCount >= 2, stallWatchOk: heartbeatCount >= 2 },
    };
  }

  async function runScenarioAction(scenarioId: string): Promise<CockpitStepResult & { fields?: ReportFields }> {
    switch (scenarioId.toUpperCase()) {
      case 'SHARE-W2N-Q': {
        await sharePattern();
        return { step: 'sharePattern', ok: true, detail: 'test-pattern publish started' };
      }
      case 'SHARE-N2W-Q': {
        const video = await waitForRemoteShareVideo();
        const stats = await measureVideoFrames(video);
        const tile = video.closest<HTMLDivElement>('.share-tile');
        const demandWidth = Number(tile?.dataset.viewerDemandPixelWidth ?? 0);
        const demandHeight = Number(tile?.dataset.viewerDemandPixelHeight ?? 0);
        // The native side shares Petal's OWN WKWebView test-pattern window, which
        // macOS throttles when self-captured (see the native N2W_LIVENESS_FPS note),
        // so delivered fps is snapshot-pull-limited rather than 30. Validate the
        // PIPELINE: the native share was received and is rendering live (frames
        // advancing) at the correct source resolution. Real third-party app windows
        // capture at full fps -- not exercised by this synthetic self-capture source.
        const demandIsKnown = demandWidth > 0 && demandHeight > 0;
        const dimsOk = demandIsKnown
          ? stats.width >= demandWidth && stats.height >= demandHeight
          : stats.width >= 900 && stats.height >= 550;
        const live = stats.fps > 0;
        const ok = dimsOk && live;
        return {
          step: 'nativeShareVideo',
          ok,
          detail: `native share received + rendering in web harness fps=${stats.fps.toFixed(1)} size=${stats.width}x${stats.height} demand=${demandWidth}x${demandHeight} (self-capture snapshot-pull limited; real windows full-fps)`,
          fields: stats,
        };
      }
      case 'CAM': {
        if (typeof cb.startCockpitWebcam !== 'function') {
          throw new CockpitInfraError('web harness is missing startCockpitWebcam callback');
        }
        const { trackName } = await cb.startCockpitWebcam();
        return {
          step: 'camera',
          ok: true,
          detail: `published webcam track ${trackName}`,
          fields: { cameraPublished: true, trackName },
        };
      }
      case 'CHAOS-DEVICE': {
        if (typeof cb.startCockpitWebcam !== 'function') {
          throw new CockpitInfraError('web harness is missing startCockpitWebcam callback');
        }
        if (typeof cb.stopCockpitWebcam !== 'function') {
          throw new CockpitInfraError('web harness is missing stopCockpitWebcam callback');
        }
        const started = await cb.startCockpitWebcam();
        await new Promise((resolve) => setTimeout(resolve, 250));
        const stopped = await cb.stopCockpitWebcam();
        const ok = stopped.stopped && stopped.trackName === started.trackName;
        return {
          step: 'cameraDisappearance',
          ok,
          detail: ok
            ? `published then unpublished synthetic camera track ${started.trackName}`
            : `synthetic camera stop did not confirm disappearance for ${started.trackName}`,
          fields: {
            cameraPublished: true,
            cameraDisappeared: ok,
            trackName: started.trackName,
          },
        };
      }
      case 'AUD': {
        if (typeof cb.startCockpitAudioTone !== 'function') {
          throw new CockpitInfraError('web harness is missing startCockpitAudioTone callback');
        }
        const { trackName } = await cb.startCockpitAudioTone();
        return {
          step: 'audio',
          ok: true,
          detail: `published synthetic audio track ${trackName}`,
          fields: { audioPublished: true, trackName },
        };
      }
      case 'AUD-N2W': {
        if (typeof cb.measureCockpitRemoteAudio !== 'function') {
          throw new CockpitInfraError('web harness is missing measureCockpitRemoteAudio callback');
        }
        const measured = await cb.measureCockpitRemoteAudio().catch((error: Error) => {
          // A browser that cannot decode remote audio is an INFRA failure, not
          // a product one -- see measureCockpitRemoteAudio's blind-instrument
          // guard and #821.
          throw new CockpitInfraError(error.message ?? String(error));
        });
        return {
          step: 'remoteAudioAudible',
          ok: measured.ok,
          detail: measured.detail,
          fields: {
            remoteAudioAudible: measured.ok,
            remoteAudioRms: measured.rms,
            remoteAudioEnergyDelta: measured.energyDelta,
            remoteAudioDurationDelta: measured.durationDelta,
            remoteAudioPublisher: measured.publisher,
          },
        };
      }
      case 'CAM-N2W': {
        if (typeof cb.measureCockpitRemoteCamera !== 'function') {
          throw new CockpitInfraError('web harness is missing measureCockpitRemoteCamera callback');
        }
        const measured = await cb.measureCockpitRemoteCamera().catch((error: Error) => {
          // A viewer that cannot read back what it drew is an INFRA failure,
          // not a product one -- see measureCockpitRemoteCamera's canvas
          // positive control and #821.
          throw new CockpitInfraError(error.message ?? String(error));
        });
        if (measured.classification === 'INFRA-FAIL') {
          // The verdict has to reach the wire as INFRA. Returning `ok: false`
          // here would arrive at the native side as a plain product failure,
          // which is exactly how a blind receiver got a P0 filed against a
          // working product (#821).
          throw new CockpitInfraError(measured.detail);
        }
        return {
          step: 'remoteCameraVisible',
          ok: measured.ok,
          detail: measured.detail,
          fields: {
            remoteCameraVisible: measured.ok,
            remoteCameraFps: measured.fps,
            remoteCameraWidth: measured.width,
            remoteCameraHeight: measured.height,
            remoteCameraFramesDecodedDelta: measured.framesDecodedDelta ?? undefined,
            remoteCameraNonBlackRatio: measured.nonBlackRatio,
            remoteCameraInterFrameDiff: measured.interFrameDiff,
            remoteCameraPublisher: measured.publisher,
          },
        };
      }
      case 'DRAW-N': {
        if (typeof cb.publishCockpitDrawStroke !== 'function') {
          throw new CockpitInfraError('web harness is missing publishCockpitDrawStroke callback');
        }
        await waitForRemoteShareVideo();
        const { windowId } = await cb.publishCockpitDrawStroke();
        return {
          step: 'drawStroke',
          ok: true,
          detail: `published draw begin/end stroke for window_id=${windowId}`,
          fields: { strokeDelivered: true, windowId },
        };
      }
      case 'TELE': {
        if (typeof cb.publishCockpitTelepointer !== 'function') {
          throw new CockpitInfraError('web harness is missing publishCockpitTelepointer callback');
        }
        await waitForRemoteShareVideo();
        const { windowId } = await cb.publishCockpitTelepointer();
        return {
          step: 'telepointer',
          ok: true,
          detail: `published telepointer movement for window_id=${windowId}`,
          fields: { telepointerMoved: true, windowId },
        };
      }
      case 'MULTI-3': {
        const roster = await waitForParticipantCount(3);
        return {
          step: 'multiPeerRoster',
          ok: true,
          detail: `observed attributable privacy-preserving MULTI-3 roster (count=${roster.participantCount}; remoteCount=${roster.remoteParticipantCount}; reporterIncluded=${roster.rosterIncludesReporter}; unique=${roster.rosterUnique}; fingerprint=${roster.rosterFingerprint})`,
          fields: roster,
        };
      }
      case 'RC-N2W': {
        // #819. The NATIVE side is the controller here; this peer publishes a
        // share for it to target, answers the control request, and records
        // what arrives. It never injects and never reports an input as
        // applied -- see remoteControlHostLedger.ts for why that distinction
        // is the whole point of this leg.
        if (!cb.enableRemoteControlHostEmulation || !cb.remoteControlHostLedger) {
          throw new CockpitInfraError(
            'this harness build has no remote-control host emulation; RC-N2W cannot be measured'
          );
        }
        cb.enableRemoteControlHostEmulation();
        await sharePattern();
        const deadline = soakClock.now() + RC_N2W_CONTROL_WAIT_MS;
        let ledger = cb.remoteControlHostLedger();
        while (soakClock.now() < deadline) {
          ledger = cb.remoteControlHostLedger();
          if (ledger.granted && ledger.kinds.includes('pointer') && ledger.kinds.includes('key')) {
            break;
          }
          await soakClock.sleep(250);
        }
        const fields = {
          controlGranted: ledger.granted,
          receivedControlKinds: ledger.kinds,
          receivedControlCount: ledger.count
        };
        if (!ledger.granted) {
          throw new CockpitInfraError(
            ledger.publishError
              ? `this peer decided to grant but publishing the status failed (${ledger.publishError}) -- the controller never heard it, so delivery is unmeasured`
              : 'no native controller requested control of this peer within the RC-N2W window, so its message delivery is unmeasured'
          );
        }
        return {
          step: 'receivedControlLedger',
          ok: ledger.kinds.length > 0,
          detail: `received ${ledger.count} control input(s) of kind(s) [${ledger.kinds.join(', ')}] from the native controller (DELIVERY only -- a browser cannot inject OS input)`,
          fields
        };
      }
      case 'RC-P1080':
        throw new CockpitInfraError(
          'RC-P1080 is scaffolded in the cockpit selector table, but the web harness does not yet force a 1080p share tier or validate remote-control injection/press-to-eye budgets'
        );
      default:
        throw new CockpitInfraError(`scenario ${scenarioId} is not implemented in web cockpit`);
    }
  }

  async function runScenario(scenarioId: string, code: string | null): Promise<CockpitScenarioResult> {
    const steps: CockpitStepResult[] = [];
    const record = async (step: CockpitStepResult & { fields?: ReportFields }) => {
      steps.push(step);
      await reportStep(scenarioId, step.step, step.ok, step.detail, step.fields);
    };
    const finish = (ok: boolean, classification: CockpitScenarioResult['classification']) => {
      const result: CockpitScenarioResult = { scenarioId, ok, classification, steps };
      hook.cockpitAutoScenario!.lastResult = result;
      return result;
    };

    // Self-check FIRST, before joining anything -- see module doc comment.
    const selfCheck = await selfCheckPatternAdvancing();
    if (!selfCheck.ok) {
      // Found live (2026-07-16, MULTI-3/CHAOS-DEVICE investigation): a
      // self-check failure here was otherwise COMPLETELY unreportable --
      // `publishReport` silently no-ops with no active room connection (see
      // its own early return on `!state.room`), and at this point we
      // haven't joined yet. The native cockpit then sees total silence and
      // can only report a generic "0 web peer reports" INFRA-FAIL, masking
      // the real, already-known reason (e.g. "frame counter advanced by 3,
      // need >= 25" -- a throttled/frozen render loop). Best-effort join
      // purely to get a working data channel to report this over; the
      // actual scenario action still never runs on this path, matching the
      // original safety intent (never publish test-pattern content from a
      // broken render loop).
      if (code) {
        try {
          await join(code);
        } catch {
          // Truly nothing to report to; `lastResult` below still carries
          // this locally for a CDP-attached caller.
        }
      }
      await record(selfCheck);
      await record({ step: 'aborted', ok: false, detail: 'self-check failed; not attempting scenario action' });
      return finish(false, 'INFRA-FAIL');
    }
    await record(selfCheck);

    if (!code) {
      await record({ step: 'join', ok: false, detail: 'no ?code= present in URL' });
      return finish(false, 'INFRA-FAIL');
    }

    try {
      await join(code);
      await record({ step: 'join', ok: true, detail: `joined via code=${code}` });
    } catch (err) {
      await record({ step: 'join', ok: false, detail: `join failed: ${(err as Error).message ?? err}` });
      return finish(false, 'TEST-FAIL');
    }

    try {
      const normalizedScenarioId = scenarioId.toUpperCase();
      const action: CockpitStepResult & { fields?: ReportFields } =
        normalizedScenarioId === 'SOAK-W2N-STALL' || normalizedScenarioId === 'SOAK-N2W-STALL'
          ? await runSoakHeartbeatWatch(scenarioId, record)
          : await runScenarioAction(scenarioId);
      await record(action);
      if (!action.ok) {
        // A scenario that RAN and failed must still send a terminal report.
        // Without one the native side waits out its whole web-report timeout
        // and concludes INFRA-FAIL "may not be implemented web-side yet" --
        // the exact opposite of the truth, and it discards the action's own
        // detail/fields (the measured numbers that say WHY it failed).
        // `scenario` is terminal on the native side regardless of ok, so the
        // failure arrives as a TEST-FAIL carrying its evidence. Measured
        // 2026-08-15 on AUD-N2W: a real, correctly-measured audio failure was
        // reported as unimplemented infrastructure.
        await record({
          step: 'scenario',
          ok: false,
          detail: `${scenarioId} failed: ${action.detail}`,
          fields: action.fields,
        });
        return finish(false, 'TEST-FAIL');
      }
      await record({ step: 'done', ok: true, detail: `${scenarioId} completed`, fields: action.fields });
    } catch (err) {
      const classification = err instanceof CockpitInfraError ? 'INFRA-FAIL' : 'TEST-FAIL';
      // The classification has to travel ON THE WIRE. `finish` only records it
      // locally, and the native side reads the published report -- so without
      // this field an instrument failure arrives as a plain `ok: false` and is
      // verdicted TEST-FAIL, i.e. blamed on the product (#821).
      await record({
        step: 'scenario',
        ok: false,
        detail: `${scenarioId} failed: ${(err as Error).message ?? err}`,
        fields: { classification },
      });
      return finish(false, classification);
    }

    return finish(true, 'PASS');
  }

  hook.cockpitAutoScenario = {
    join,
    sharePattern,
    runScenario,
    lastResult: null,
  };

  function maybeRunAutoScenario() {
    const params = new URLSearchParams(location.search);
    const auto = params.get('auto');
    if (!auto) return;
    const code = params.get('code');
    void runScenario(auto, code);
  }

  return { maybeRunAutoScenario, runScenario };
}
