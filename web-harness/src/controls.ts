import { Room, Track, LocalVideoTrack, LocalAudioTrack } from 'livekit-client';
import type { HarnessContext } from './context.ts';
import {
  deviceChanged,
  isPermissionDeniedError,
  noteLeaveRequested,
  permissionDenied,
  shareStarted,
  shareStopped,
} from './analytics.ts';
import { accessCodeForCredential, generateMeetingCode, slugify } from '@petal/shared/logic/meetingCode';
import { runWebMeetingAction, submitWebCreateJoinAction } from './createJoinAction.ts';
import type { MeetingAction } from './meetingActionError.ts';
import {
  identityPaletteIndexFromMetadata,
  mergeSharedSourceMetadata,
  trackNameForWindow,
  trackNameForCamera,
  cameraWindowId,
  randomWindowId,
} from './trackNames.ts';
import {
  CAMERA_VIDEO_CONSTRAINTS,
  CAMERA_VIDEO_ENCODING,
  TEST_PATTERN_SCREENSHARE_ENCODING,
  screenSharePublishEncoding,
  HARNESS_AUDIO_INPUT_STORAGE_KEY,
  HARNESS_AUDIO_OUTPUT_STORAGE_KEY,
  HARNESS_VIDEO_INPUT_STORAGE_KEY,
  HARNESS_COLOR_STORAGE_KEY,
  HARNESS_DEBUG_MODE_STORAGE_KEY,
  HARNESS_LOCAL_ECHO_STORAGE_KEY,
  HARNESS_NAME_STORAGE_KEY,
} from './constants.ts';
import { participantDisplayName } from './tiles.ts';
import { audioReceiverTelemetryFromStatsReport } from './audioReceiverTelemetry.ts';
import { framesDecodedFromStatsReport } from './cameraDecodeHealth.ts';
import {
  BLACK_FLOOR,
  CAMERA_SAMPLE_HEIGHT,
  CAMERA_SAMPLE_WIDTH,
  evaluateRemoteCameraTile,
} from './cameraFrameAdvance.ts';
import { AUDIBILITY_RMS_BAR, assertRemoteAudioOraclesAgree } from './audioOracleAgreement.ts';
import { setRoomDisplayLabel } from './roomLabels.ts';
import { inviteLinkCopiedToastMessage } from './inviteToast.ts';
import type { FeedbackReportController } from './feedbackReport.ts';
import { PresentationSourceHost } from './presentationSourceHost.ts';
import { setupDeviceMenu } from './deviceMenu.ts';

export { newMeetingCredentialFromInput } from './createJoinAction.ts';

// ---------------------------------------------------------------------------
// Control bar + dev-panel wiring: leave/invite, real mic, real webcam, real
// screen share, the synthetic test-pattern share, and the synthetic 440Hz
// tone. Also the unified Create/Join submit flow.
// ---------------------------------------------------------------------------
export function inviteLinkForCredential(credential: string, origin = location.origin, displayName?: string | null): string {
  const accessCode = accessCodeForCredential(credential);
  if (!accessCode) return `${origin}/`;
  const label = displayName?.trim() ? slugify(displayName) : null;
  return label
    ? `${origin}/${encodeURIComponent(label)}/${encodeURIComponent(accessCode)}`
    : `${origin}/${encodeURIComponent(accessCode)}`;
}

export const HARNESS_IDENTITY_STORAGE_KEY = 'petal:harness-identity:v1';

/**
 * How long CAM-N2W waits for the receiving tile to have a decoded frame in it
 * before giving up on the early pixel sample. Generous on purpose: the cost of
 * waiting is a slower scenario, and the cost of not waiting is an INFRA-FAIL
 * against a product that was merely still coming up.
 */
const FIRST_SAMPLE_WAIT_MS = 5000;

/**
 * The real microphone's LiveKit track name. Cross-side contract with the
 * native app's `transport::audio::MIC_TRACK_NAME` — see `docs/CONTRACTS.md`'s
 * "Microphone track" section and the `micTrack` fixture in
 * `contracts/petal-contracts.json`, which `tests/contracts.test.ts` pins
 * against this constant.
 *
 * Exported (rather than left as an inline literal at the publish call) so the
 * contract test has something to import: before #787 both sides hard-coded
 * `'petal-mic'` independently with nothing pinning them together, and
 * `transport/audio.rs`'s own comment claimed it was documented in
 * CONTRACTS.md when it was not.
 *
 * NOTE — the mic track *name* is the contract; the audio publish OPTIONS are
 * not symmetric. Native passes `red: false` explicitly; this side passes
 * nothing and gets livekit-client's `red: true` default. That divergence is
 * real and documented in CONTRACTS.md; it is #787 hypothesis B and must not be
 * "fixed" here without evidence.
 */
export const MIC_TRACK_NAME = 'petal-mic';

function randomIdentitySuffix(): string {
  return globalThis.crypto?.randomUUID?.() ?? String(Math.floor(Math.random() * 1_000_000_000));
}

export function displayNameFromInput(value: string): string {
  return value.trim() || 'Guest';
}

export function tokenRequestBody(meetingCode: string, identity: string, displayName: string) {
  // The access code is only known when this session derived the credential
  // from an invite (join link / typed code / own create); it is what the
  // backend demands for a room stamped `open: false` and ignores otherwise
  // (docs/CONTRACTS.md "Closed rooms and removed participants"). Omitted, not
  // null, when unknown so open-room requests are byte-identical to before.
  const accessCode = accessCodeForCredential(meetingCode);
  return {
    room: meetingCode,
    identity,
    displayName,
    ...(accessCode ? { accessCode } : {}),
  };
}

export function supportsAudioOutputSelection(): boolean {
  return (
    typeof HTMLMediaElement !== 'undefined' &&
    'setSinkId' in HTMLMediaElement.prototype
  );
}

export function resolvePersistedDeviceId(devices: Pick<MediaDeviceInfo, 'deviceId'>[], storedId: string | null): string {
  const trimmed = storedId?.trim();
  if (!trimmed) return '';
  return devices.some((device) => device.deviceId === trimmed) ? trimmed : '';
}

export function audioConstraintForDeviceId(deviceId: string): boolean | MediaTrackConstraints {
  return deviceId ? { deviceId: { ideal: deviceId } } : true;
}

export function videoConstraintsForDeviceId(deviceId: string): MediaTrackConstraints {
  if (!deviceId) return { ...CAMERA_VIDEO_CONSTRAINTS };
  return { ...CAMERA_VIDEO_CONSTRAINTS, deviceId: { ideal: deviceId } };
}

export function shouldShowFirstVisitOnboarding(storage: Pick<Storage, 'getItem'>): boolean {
  return !storage.getItem(HARNESS_IDENTITY_STORAGE_KEY)?.trim() && !storage.getItem(HARNESS_COLOR_STORAGE_KEY)?.trim();
}

export function resolveHarnessIdentity(
  displayNameInput: Pick<HTMLInputElement, 'value'>,
  storage: Pick<Storage, 'getItem' | 'setItem'>,
  suffix: () => string = randomIdentitySuffix
): string {
  const displayName = displayNameFromInput(displayNameInput.value);
  displayNameInput.value = displayName;
  storage.setItem(HARNESS_NAME_STORAGE_KEY, displayName);

  const existing = storage.getItem(HARNESS_IDENTITY_STORAGE_KEY)?.trim();
  if (existing) return existing;
  const identity = `web-${suffix()}`;
  storage.setItem(HARNESS_IDENTITY_STORAGE_KEY, identity);
  return identity;
}

export function setupControls(ctx: HarnessContext, feedbackReport?: FeedbackReportController) {
  const { dom, state, cb } = ctx;
  const {
    displayNameInput,
    meetingCodeInput,
    joinBtn,
    ctlAudio,
    ctlVideo,
    ctlShare,
    ctlInvite,
    roomNameEl,
    roomCopyButton,
    roomRenameButton,
    ctlLeave,
    shareBtn,
    canvas,
    audioCaret,
    videoCaret,
    devicesMenu,
    devicesMenuTitle,
    devicesMenuBody,
    micCheckbox,
    localEchoCheckbox,
    debugModeCheckbox,
    cameraTrackNameDisplay,
  } = dom;
  const {
    logEvent,
    clearError,
    showError,
    showToast,
    setScreenShareState,
    setShareState,
    setMicState,
    setRealMicState,
    setWebcamState,
    setAudioControl,
    setVideoControl,
    setShareControl,
  } = ctx.ui;
  let presentationSourceHost: PresentationSourceHost | null = null;

  function removePresentationSourceHost(): void {
    presentationSourceHost?.unmount();
    presentationSourceHost = null;
  }

  function resolveIdentity(): string {
    return resolveHarnessIdentity(displayNameInput, localStorage);
  }

  async function getLocalDevices(kind: MediaDeviceKind): Promise<MediaDeviceInfo[]> {
    try {
      return await Room.getLocalDevices(kind, false);
    } catch (err) {
      logEvent(`could not list ${kind} devices: ${(err as Error).message ?? err}`, 'warn');
      return [];
    }
  }

  async function persistListedDevice(
    kind: MediaDeviceKind,
    storageKey: string
  ): Promise<string> {
    const devices = await getLocalDevices(kind);
    const storedId = localStorage.getItem(storageKey);
    const selectedId = resolvePersistedDeviceId(devices, storedId);
    if (selectedId) localStorage.setItem(storageKey, selectedId);
    else if (devices.length > 0) localStorage.removeItem(storageKey);
    return selectedId;
  }

  async function refreshMeetingDevices() {
    await persistListedDevice('audioinput', HARNESS_AUDIO_INPUT_STORAGE_KEY);
    await persistListedDevice('videoinput', HARNESS_VIDEO_INPUT_STORAGE_KEY);
    if (!supportsAudioOutputSelection()) return;
    const outputId = await persistListedDevice('audiooutput', HARNESS_AUDIO_OUTPUT_STORAGE_KEY);
    if (state.room && outputId) {
      try {
        await switchActiveDevice('audiooutput', outputId);
      } catch (err) {
        logEvent(`stored speaker device could not be applied: ${(err as Error).message ?? err}`, 'warn');
      }
    }
  }

  async function switchActiveDevice(kind: MediaDeviceKind, deviceId: string) {
    if (!state.room) return;
    await state.room.switchActiveDevice(kind, deviceId, false);
  }

  async function applyAudioInputDevice(deviceId: string) {
    if (deviceId) localStorage.setItem(HARNESS_AUDIO_INPUT_STORAGE_KEY, deviceId);
    else localStorage.removeItem(HARNESS_AUDIO_INPUT_STORAGE_KEY);
    if (state.realMicOn) {
      await switchActiveDevice('audioinput', deviceId);
      deviceChanged('mic', 'switched');
    }
    logEvent(deviceId ? 'microphone device selected' : 'microphone reset to system default', 'ok');
  }

  async function applyAudioOutputDevice(deviceId: string) {
    if (!supportsAudioOutputSelection()) return;
    if (deviceId) localStorage.setItem(HARNESS_AUDIO_OUTPUT_STORAGE_KEY, deviceId);
    else localStorage.removeItem(HARNESS_AUDIO_OUTPUT_STORAGE_KEY);
    await switchActiveDevice('audiooutput', deviceId);
    logEvent(deviceId ? 'speaker device selected' : 'speaker reset to system default', 'ok');
  }

  async function applyVideoInputDevice(deviceId: string) {
    if (deviceId) localStorage.setItem(HARNESS_VIDEO_INPUT_STORAGE_KEY, deviceId);
    else localStorage.removeItem(HARNESS_VIDEO_INPUT_STORAGE_KEY);
    if (state.webcamOn) await switchActiveDevice('videoinput', deviceId);
    logEvent(deviceId ? 'camera device selected' : 'camera reset to system default', 'ok');
  }

  function renameRoomDisplayName(code: string, displayName: string | null): string {
    return setRoomDisplayLabel(code, displayName);
  }

  function credentialForNewMeeting(label?: string): string {
    return generateMeetingCode(label);
  }

  function pendingRecentCredential(): string | null {
    const credential = meetingCodeInput.dataset.petalRoomCredential;
    if (!credential) return null;
    const label = meetingCodeInput.dataset.petalRoomDisplayLabel;
    if (label && meetingCodeInput.value.trim() === label) return credential;
    delete meetingCodeInput.dataset.petalRoomCredential;
    delete meetingCodeInput.dataset.petalRoomDisplayLabel;
    return null;
  }

  function clearSubmittedMeetingInput(code: string) {
    if (state.currentMeetingCode !== code || !state.room) return;
    meetingCodeInput.value = '';
    delete meetingCodeInput.dataset.petalRoomCredential;
    delete meetingCodeInput.dataset.petalRoomDisplayLabel;
    cb.updateUnifiedCtaLabel();
  }

  async function connectWithCredential(code: string) {
    await cb.connectToMeeting(code, resolveIdentity());
    await refreshMeetingDevices();
    clearSubmittedMeetingInput(code);
  }

  async function runMeetingAction(action: MeetingAction, task: () => Promise<void>) {
    await runWebMeetingAction(action, task, { showError, showToast, logEvent });
  }

  async function submitMeetingField() {
    await submitWebCreateJoinAction({
      clearError,
      connectWithCredential,
      credentialForNewMeeting,
      logEvent,
      pendingRecentCredential,
      rawInput: meetingCodeInput.value,
      renameRoomDisplayName,
      showError,
      showToast
    });
  }

  async function copyCurrentInviteLink() {
    if (!state.currentMeetingCode) return;
    const url = inviteLinkForCredential(
      state.currentMeetingCode,
      location.origin,
      cb.roomDisplayLabelForCredential(state.currentMeetingCode)
    );
    try {
      await navigator.clipboard.writeText(url);
      showToast(inviteLinkCopiedToastMessage(url));
      logEvent(`invite link copied: ${url}`, 'ok');
    } catch {
      // Clipboard API unavailable (e.g. insecure context) -- surface the link
      // in the toast + log instead of failing silently.
      showToast(inviteLinkCopiedToastMessage(url));
      logEvent(`clipboard unavailable -- invite link: ${url}`, 'warn');
    }
  }

  function startRoomRename() {
    const code = state.currentMeetingCode;
    if (!code || roomNameEl.querySelector('input')) return;

    const previousLabel = cb.roomDisplayLabelForCredential(code);
    const input = document.createElement('input');
    input.className = 'room-name-input';
    input.type = 'text';
    input.autocapitalize = 'off';
    input.value = previousLabel;
    input.setAttribute('aria-label', 'Room display name');

    let finished = false;
    const finish = (commit: boolean) => {
      if (finished) return;
      finished = true;
      const nextLabel = commit ? cb.renameRoomDisplayName(code, input.value) : previousLabel;
      roomNameEl.textContent = nextLabel;
      roomNameEl.classList.remove('renaming');
      roomRenameButton.disabled = false;
      roomCopyButton.disabled = false;
      cb.refreshRecentRooms();
      if (commit) {
        showToast(input.value.trim() ? 'Room renamed' : 'Room name reset');
        logEvent(`room display name ${input.value.trim() ? 'renamed' : 'reset'} for ${code}`, 'ok');
      }
    };

    input.addEventListener('keydown', (event) => {
      if (event.key === 'Enter') {
        event.preventDefault();
        finish(true);
      } else if (event.key === 'Escape') {
        event.preventDefault();
        finish(false);
      }
    });
    input.addEventListener('blur', () => finish(true));

    roomNameEl.replaceChildren(input);
    roomNameEl.classList.add('renaming');
    roomRenameButton.disabled = true;
    roomCopyButton.disabled = true;
    input.focus();
    input.select();
  }

  async function startTestPatternShare(): Promise<void> {
    if (!state.room) throw new Error('sharePattern requires an active room -- join first');
    if (state.sharing) return; // idempotent: already sharing
    feedbackReport?.onShareStartIntent();

    cb.startCanvasAnimation();
    try {
      presentationSourceHost = new PresentationSourceHost(canvas);
      presentationSourceHost.mount();
      const stream = canvas.captureStream(30);
      const videoTrack = stream.getVideoTracks()[0];
      videoTrack.contentHint = 'detail';
      state.localVideoTrack = new LocalVideoTrack(videoTrack);
      await state.room.localParticipant.publishTrack(state.localVideoTrack, {
        name: trackNameForWindow(ctx.windowId),
        source: Track.Source.ScreenShare,
        // H.264 is load-bearing -- see verifyH264Negotiated's doc comment.
        videoCodec: 'h264',
        // Override livekit's default ScreenShare preset (15fps, tuned for
        // slides) so the animated pattern publishes at 30fps -- otherwise
        // SHARE-W2N-Q's fps>20 gate can never pass. (#254 fps ceiling.)
        screenShareEncoding: TEST_PATTERN_SCREENSHARE_ENCODING,
        frameMetadata: { timestamp: true, frameId: true },
      });
      // Same metadata the real-screen path publishes: without the kind+scale
      // entry the native receiver never offers remote control for this share
      // (#819 review -- RC-N2W preflighted itself into refusing every run).
      await setLocalParticipantMetadata(
        state.room.localParticipant,
        mergeSharedSourceMetadata(state.room.localParticipant.metadata, ctx.windowId, 'window')
      );
      cb.syncHarnessHook();
      void cb.verifyH264Negotiated(state.localVideoTrack, `test-pattern share (window_id=${ctx.windowId})`);
      state.sharing = true;
      setShareState(`sharing (window_id=${ctx.windowId})`, true);
      shareBtn.textContent = 'Stop test pattern';
      logEvent(`started sharing test pattern window_id=${ctx.windowId}`, 'ok');
      shareStarted('window');
      cb.addShareTile(
        state.room.localParticipant.identity,
        true,
        'testpattern',
        state.localVideoTrack,
        participantDisplayName(state.room.localParticipant.identity, displayNameInput.value),
        ctx.windowId
      );
      cb.startTelepointerSender();
      logEvent('telepointer sender started (animated cursor over shared window)', 'ok');
    } catch (err) {
      removePresentationSourceHost();
      feedbackReport?.onShareEnded();
      showError(`Publish failed: ${(err as Error).message ?? err}`);
      throw err;
    }
  }

  async function startCockpitWebcam(): Promise<{ trackName: string }> {
    if (!state.room) throw new Error('camera requires an active room -- join first');
    const name = trackNameForCamera(state.room.localParticipant.identity);
    if (state.webcamOn && state.localCameraTrack) return { trackName: name };

    // Synthetic camera source, matching startCockpitAudioTone's synthetic
    // 440Hz tone rather than a real getUserMedia() call: a real camera
    // requires physical hardware AND a pre-granted permission that can't be
    // clicked through headlessly, and getUserMedia can hang indefinitely
    // with no prompt and no rejection if the camera is already held by
    // another app (see the manual UI camera button below, which has to race
    // a 10s timeout against exactly this). The cockpit must never depend on
    // real hardware or be able to hang -- see the manual button's comment
    // for the live-observed hang case this avoids entirely.
    logEvent('cockpit: starting synthetic camera canvas for CAM scenario');
    setWebcamState('starting synthetic camera...', false);
    const canvas = document.createElement('canvas');
    canvas.width = 640;
    canvas.height = 480;
    const ctx2d = canvas.getContext('2d')!;
    let frame = 0;
    const drawFrame = () => {
      frame += 1;
      ctx2d.fillStyle = '#1b1033';
      ctx2d.fillRect(0, 0, canvas.width, canvas.height);
      ctx2d.fillStyle = '#aa3bff';
      const angle = (frame / 30) % (2 * Math.PI);
      const cx = canvas.width / 2 + Math.cos(angle) * 120;
      const cy = canvas.height / 2 + Math.sin(angle) * 80;
      ctx2d.beginPath();
      ctx2d.arc(cx, cy, 24, 0, 2 * Math.PI);
      ctx2d.fill();
      ctx2d.fillStyle = '#ffffff';
      ctx2d.font = '20px sans-serif';
      ctx2d.fillText(`cockpit CAM frame ${frame}`, 16, 32);
    };
    drawFrame();
    state.syntheticCameraIntervalId = setInterval(drawFrame, 1000 / 24);
    const canvasStream = canvas.captureStream(24);
    const camTrack = canvasStream.getVideoTracks()[0];
    state.localCameraTrack = new LocalVideoTrack(camTrack);
    try {
      await state.room.localParticipant.publishTrack(state.localCameraTrack, {
        name,
        source: Track.Source.Camera,
        videoCodec: 'h264',
        videoEncoding: CAMERA_VIDEO_ENCODING,
        // One encoding removes the lowest-layer SFU ramp; maintaining
        // resolution also stops Chrome's single-encoding quality scaler from
        // starting at 360p while sender bandwidth estimation warms up.
        simulcast: false,
        degradationPreference: 'maintain-resolution',
      });
      state.webcamOn = true;
      setVideoControl(true);
      setWebcamState(`on (${name})`, true);
      cameraTrackNameDisplay.textContent = name;
      logEvent(`cockpit: started publishing webcam as "${name}"`, 'ok');
      cb.setTileCamera(state.room.localParticipant.identity, true, state.localCameraTrack, cameraWindowId(name));
      void cb.verifyH264Negotiated(state.localCameraTrack, `webcam (${name})`);
      return { trackName: name };
    } catch (err) {
      if (state.syntheticCameraIntervalId !== null) {
        clearInterval(state.syntheticCameraIntervalId);
        state.syntheticCameraIntervalId = null;
      }
      camTrack.stop();
      state.localCameraTrack = null;
      setWebcamState('off', false);
      throw err;
    }
  }

  async function stopCockpitWebcam(): Promise<{ trackName: string; stopped: boolean }> {
    if (!state.room) throw new Error('camera stop requires an active room -- join first');
    const name = trackNameForCamera(state.room.localParticipant.identity);
    const track = state.localCameraTrack;
    if (!state.webcamOn || !track) return { trackName: name, stopped: false };

    logEvent(`cockpit: stopping synthetic camera "${name}" for CHAOS-DEVICE`);
    if (state.syntheticCameraIntervalId !== null) {
      clearInterval(state.syntheticCameraIntervalId);
      state.syntheticCameraIntervalId = null;
    }
    await state.room.localParticipant.unpublishTrack(track, true);
    state.localCameraTrack = null;
    state.webcamOn = false;
    setVideoControl(false);
    setWebcamState('off', false);
    cameraTrackNameDisplay.textContent = name;
    cb.clearTileCamera(state.room.localParticipant.identity);
    return { trackName: name, stopped: true };
  }

  /**
   * Record `windowMs` of a live audio track and return the RMS of the decoded
   * waveform, normalised to [0,1]. Works headless: MediaRecorder and
   * `decodeAudioData` both run without an output device, unlike an
   * AnalyserNode fed from a remote WebRTC track.
   */
  async function recordRemoteAudioRms(
    mediaStreamTrack: MediaStreamTrack,
    windowMs: number
  ): Promise<{ rms: number; detail: string }> {
    if (typeof MediaRecorder === 'undefined') {
      throw new Error('MediaRecorder unavailable in this browser');
    }
    const stream = new MediaStream([mediaStreamTrack]);
    const chunks: Blob[] = [];
    const recorder = new MediaRecorder(stream);
    recorder.ondataavailable = (event) => {
      if (event.data && event.data.size > 0) chunks.push(event.data);
    };
    const stopped = new Promise<void>((resolve) => {
      recorder.onstop = () => resolve();
    });
    recorder.start();
    await new Promise((resolve) => setTimeout(resolve, windowMs));
    recorder.stop();
    await stopped;
    const blob = new Blob(chunks);
    if (blob.size === 0) throw new Error('recorder produced no data');
    const bytes = await blob.arrayBuffer();
    const ctx = new AudioContext();
    try {
      const buffer = await ctx.decodeAudioData(bytes);
      let sumSquares = 0;
      let count = 0;
      for (let channel = 0; channel < buffer.numberOfChannels; channel += 1) {
        const data = buffer.getChannelData(channel);
        for (const sample of data) {
          sumSquares += sample * sample;
          count += 1;
        }
      }
      const rms = count > 0 ? Math.sqrt(sumSquares / count) : 0;
      return {
        rms,
        detail: `${buffer.duration.toFixed(2)}s recorded, ${count} samples`,
      };
    } finally {
      await ctx.close().catch(() => undefined);
    }
  }

  /**
   * #812 / journey AUD-04: the reverse of AUD-01 -- does the NATIVE mic
   * actually arrive here as AUDIBLE audio?
   *
   * The oracle is `totalAudioEnergy / totalSamplesDuration` from the
   * browser's own inbound-rtp stats: the RMS amplitude of the samples the
   * decoder actually produced, measured by Chrome after decode and after
   * concealment. Never a packet or byte counter -- #787 is the standing proof
   * that bytes flow while the listener hears silence, and a publisher-side
   * counter would have called that bug green.
   *
   * An AnalyserNode was the obvious first choice and is the wrong one here:
   * in the headless Chrome the cockpit drives, a MediaStreamAudioSourceNode
   * built from a remote WebRTC track reads a flat 0 no matter how loud the
   * stream is (measured 2026-08-15: peak=0/128 over 40 reads while the SAME
   * track's stats reported real energy). A silence-shaped instrument failure
   * is the single most dangerous thing this scenario could contain, so the
   * analyser is gone rather than kept as a "secondary signal".
   *
   * Deltas, not totals: the counters are cumulative from subscribe, so a
   * long-silent-then-loud track and a loud-then-silent one look identical in
   * absolute terms. Sampling a window measures what is arriving NOW.
   */
  async function measureCockpitRemoteAudio(
    windowMs = 4000
  ): Promise<{
    ok: boolean;
    rms: number;
    energyDelta: number;
    durationDelta: number;
    trackSid: string;
    publisher: string;
    detail: string;
  }> {
    if (!state.room) throw new Error('remote audio measurement requires an active room -- join first');

    type StatsCapableTrack = {
      mediaStreamTrack?: MediaStreamTrack;
      getRTCStatsReport?: () => Promise<RTCStatsReport | undefined>;
    };
    const readEnergy = async (
      track: StatsCapableTrack
    ): Promise<{
      energy: number;
      duration: number;
      packets: number;
      decodedSamples: number;
    } | null> => {
      if (typeof track.getRTCStatsReport !== 'function') return null;
      const report = await track.getRTCStatsReport().catch(() => undefined);
      if (!report) return null;
      const summary = audioReceiverTelemetryFromStatsReport(report);
      if (!summary) return null;
      const { totalAudioEnergy, totalSamplesDuration, packetsReceived, totalSamplesReceived } =
        summary;
      if (totalAudioEnergy === null || totalSamplesDuration === null) return null;
      return {
        energy: totalAudioEnergy,
        duration: totalSamplesDuration,
        packets: packetsReceived ?? 0,
        decodedSamples: totalSamplesReceived ?? 0,
      };
    };

    let track: StatsCapableTrack | null = null;
    let trackSid = '';
    let publisher = '';
    // A publication the SFU still considers MUTED delivers digital silence
    // that is indistinguishable from a broken encoder at every counter, so
    // the mute state has to be reported with the measurement, not inferred.
    let publicationMuted = false;
    let publication: { isMuted?: boolean } | null = null;
    let mediaTrackState = 'unknown';
    const deadline = Date.now() + 15000;
    while (Date.now() < deadline && !track) {
      for (const participant of state.room.remoteParticipants.values()) {
        for (const pub of participant.trackPublications.values()) {
          if (pub.kind !== Track.Kind.Audio) continue;
          const remote = pub.track as StatsCapableTrack | undefined;
          if (!remote?.mediaStreamTrack) continue;
          track = remote;
          trackSid = pub.trackSid;
          publisher = participant.identity;
          publicationMuted = Boolean(pub.isMuted);
          publication = pub as { isMuted?: boolean };
          mediaTrackState = `readyState=${remote.mediaStreamTrack?.readyState} muted=${remote.mediaStreamTrack?.muted} enabled=${remote.mediaStreamTrack?.enabled}`;
          break;
        }
        if (track) break;
      }
      if (!track) await new Promise((resolve) => setTimeout(resolve, 500));
    }
    if (!track) {
      return {
        ok: false,
        rms: 0,
        energyDelta: 0,
        durationDelta: 0,
        trackSid: '',
        publisher: '',
        detail: 'no subscribed remote audio track appeared within 15s',
      };
    }

    // Primary oracle: RECORD the received track and decode the recording.
    // MediaRecorder pulls the decoded audio through a completely different
    // path than the inbound-rtp counters, so it settles the question the
    // counters cannot: `totalAudioEnergy` can read 0 for a perfectly audible
    // stream (it is partly derived from the RTP audio-level header
    // extension, which a native publisher need not send), and a headless
    // AnalyserNode on a remote track reads a flat 0 regardless. Only the
    // recorded samples are the audio itself.
    //
    // #822: recording and stats must cover the SAME window. Sequential
    // record-then-stats compared two different slices of audio, so a
    // contradiction across the audibility bar was a quiet product `ok`.
    const before = await readEnergy(track);
    if (!before) {
      // Cannot listen != heard nothing. Throwing makes this an infra failure
      // instead of a false "silence" verdict against the product.
      throw new Error(
        `remote audio track ${trackSid} exposes no inbound-rtp audio-energy stats -- cannot measure audibility`
      );
    }
    const [recorded, after] = await Promise.all([
      recordRemoteAudioRms(track.mediaStreamTrack as MediaStreamTrack, windowMs).catch(
        (error: Error) => ({ rms: -1, detail: `recording failed: ${error.message}` })
      ),
      (async () => {
        await new Promise((resolve) => setTimeout(resolve, windowMs));
        return readEnergy(track);
      })(),
    ]);
    if (!after) {
      throw new Error(`remote audio stats disappeared mid-measurement for track ${trackSid}`);
    }

    const energyDelta = Math.max(0, after.energy - before.energy);
    const durationDelta = Math.max(0, after.duration - before.duration);
    const packetsDelta = Math.max(0, after.packets - before.packets);
    const decodedDelta = Math.max(0, after.decodedSamples - before.decodedSamples);
    // "Could not listen" is NOT "heard nothing". Packets arriving while the
    // decoder emits zero samples means this browser never decoded the stream
    // at all -- headless Chrome does exactly that (no audio output device, so
    // `jitterBufferEmittedCount` and `totalSamplesReceived` stay 0 while
    // `totalSamplesDuration` advances on the playout clock). Measured
    // 2026-08-15: a native tone that a real Chrome renders at rms 0.35, and a
    // native subscriber hears at RMS 11528, read EXACTLY 0.0000 here through
    // both the stats and a MediaRecorder capture. That silence-shaped
    // instrument failure produced a P0 bug report against a working product
    // (#821). It must fail loudly as infrastructure, never as "silence".
    // Exact-zero is not a strict enough test: a decoder that emits a burst and
    // stalls, or one whose sample counter advances only through concealment,
    // would slip past it into the false-"product failed" bucket. Require the
    // decoder to have produced most of the window it was asked about.
    const expectedSamples = (windowMs / 1000) * 48000;
    if (packetsDelta > 0 && decodedDelta < expectedSamples * 0.25) {
      throw new Error(
        `received ${packetsDelta} RTP packets but the decoder emitted only ${decodedDelta} samples (expected ~${Math.round(expectedSamples)}) -- this browser is not decoding remote audio (headless Chrome cannot; run the audio peer headed). Cannot measure audibility.`
      );
    }
    // Counters that went BACKWARDS mean the stats were reset mid-window (a
    // resubscribe): the deltas below would clamp to 0 and read as silence.
    if (
      after.packets < before.packets ||
      after.decodedSamples < before.decodedSamples ||
      after.duration < before.duration
    ) {
      throw new Error(
        'receiver stats reset mid-measurement (track resubscribed) -- the window is not measurable'
      );
    }
    // The publication was muted for some/all of the window. A muted track is
    // digital silence by design, so an audibility verdict against it says
    // nothing about the product -- Petal joins muted, and that is exactly the
    // trap that made every local #821 measurement meaningless.
    // Read the mute state again AFTER the window: a publication that was muted
    // only at discovery (the unmute still in flight) measured fine and must not
    // be rejected.
    const mutedAtEnd = Boolean(publication?.isMuted);
    if (publicationMuted && mutedAtEnd) {
      throw new Error(
        `the remote publication is MUTED for the whole window -- unmute before measuring audibility (track=${trackSid})`
      );
    }
    const statsRms = durationDelta > 0 ? Math.sqrt(energyDelta / durationDelta) : 0;
    // The recorded waveform decides; the stats counter is reported alongside
    // it as corroboration (and as the record of how far the two disagree).
    assertRemoteAudioOraclesAgree(recorded.rms, statsRms, AUDIBILITY_RMS_BAR);
    const rms = recorded.rms >= 0 ? recorded.rms : statsRms;
    // A half-scale 440Hz tone measures rms ~0.35; a muted/silent track
    // measures 0, and Opus comfort noise stays around 1e-3. 0.01 sits two
    // orders of magnitude below the tone and an order above the noise.
    const ok = durationDelta > 0.5 && rms >= AUDIBILITY_RMS_BAR;
    return {
      ok,
      rms: Number(rms.toFixed(4)),
      energyDelta: Number(energyDelta.toFixed(4)),
      durationDelta: Number(durationDelta.toFixed(3)),
      trackSid,
      publisher,
      detail: `received audio from '${publisher}' track=${trackSid}: rms=${rms.toFixed(4)} (recorded waveform${recorded.rms < 0 ? ` UNAVAILABLE: ${recorded.detail}` : ''}; inbound-rtp stats rms=${statsRms.toFixed(4)}) over ${durationDelta.toFixed(2)}s of decoded samples in a ${windowMs}ms window (${packetsDelta} RTP packets; publicationMuted=${publicationMuted}; ${mediaTrackState}; bar: rms>=0.01 with >0.5s decoded)`,
    };
  }

  /**
   * #815 / journey CAM-05: the reverse of CAM -- does the NATIVE camera
   * actually arrive here as a VISIBLE tile?
   *
   * The oracle is the drawn pixels plus the frame-advance counters, never
   * "a track was subscribed". #806 is the standing lesson that a tile can be
   * subscribed, decoding, and still black on screen; a subscription-shaped
   * check calls that green.
   *
   * The canvas positive control is the other half. A blind readback path
   * (tainted canvas, no 2d context, a compositor handing back empty buffers)
   * returns exactly the all-zero pixels a genuinely black tile does. #821 is
   * what that ambiguity costs when it is resolved the wrong way: a P0 filed
   * against a working product. So a known white rectangle goes through the
   * SAME canvas first, and only if it reads back white is "the tile is black"
   * allowed to become a verdict about the product.
   *
   * Deltas, not totals: `framesDecoded` is cumulative from subscribe, so a
   * stream that ran and then froze looks identical to a live one in absolute
   * terms. Sampling a window measures what is arriving NOW.
   */
  async function measureCockpitRemoteCamera(windowMs = 4000): Promise<{
    ok: boolean;
    classification: 'PASS' | 'TEST-FAIL' | 'INFRA-FAIL';
    fps: number;
    width: number;
    height: number;
    framesDecodedDelta: number | null;
    nonBlackRatio: number;
    interFrameDiff: number;
    trackSid: string;
    publisher: string;
    detail: string;
  }> {
    if (!state.room) {
      throw new Error('remote camera measurement requires an active room -- join first');
    }

    type StatsCapableTrack = {
      mediaStreamTrack?: MediaStreamTrack;
      getRTCStatsReport?: () => Promise<RTCStatsReport | undefined>;
    };
    const readFramesDecoded = async (track: StatsCapableTrack): Promise<number | null> => {
      if (typeof track.getRTCStatsReport !== 'function') return null;
      const report = await track.getRTCStatsReport().catch(() => undefined);
      if (!report) return null;
      return framesDecodedFromStatsReport(report);
    };

    let track: StatsCapableTrack | null = null;
    let trackSid = '';
    let publisher = '';
    const deadline = Date.now() + 15000;
    while (Date.now() < deadline && !track) {
      for (const participant of state.room.remoteParticipants.values()) {
        for (const pub of participant.trackPublications.values()) {
          if (pub.kind !== Track.Kind.Video) continue;
          if (!(pub.trackName ?? '').startsWith('petal-camera-')) continue;
          const remote = pub.track as StatsCapableTrack | undefined;
          if (!remote?.mediaStreamTrack) continue;
          track = remote;
          trackSid = pub.trackSid;
          publisher = participant.identity;
          break;
        }
        if (track) break;
      }
      if (!track) await new Promise((resolve) => setTimeout(resolve, 500));
    }
    if (!track) {
      // NOT an infra throw. The native arm reads its own SEND camera
      // telemetry for exactly this case, so "nothing was published, or
      // nothing was delivered" is a real product signal there -- unlike a
      // blind instrument here, which is what INFRA-FAIL is reserved for.
      return {
        ok: false,
        classification: 'TEST-FAIL',
        fps: 0,
        width: 0,
        height: 0,
        framesDecodedDelta: null,
        nonBlackRatio: 0,
        interFrameDiff: 0,
        trackSid: '',
        publisher: '',
        detail: 'no remote petal-camera-* publication was subscribed within 15s',
      };
    }

    const mediaStreamTrack = track.mediaStreamTrack as MediaStreamTrack;
    const video = Array.from(document.querySelectorAll<HTMLVideoElement>('video')).find(
      (candidate) => {
        const stream = candidate.srcObject as MediaStream | null;
        return Boolean(stream?.getTracks?.().includes(mediaStreamTrack));
      }
    );
    if (!video) {
      // The harness renders every subscribed camera into a tile, so a
      // subscribed track with no element attached is a harness fault. Judging
      // the product on a tile that was never mounted would be exactly the
      // blind-instrument mistake this oracle exists to avoid.
      throw new Error(
        `remote camera track ${trackSid} from '${publisher}' is subscribed but attached to no <video> element -- the harness cannot see it`
      );
    }
    const requestVideoFrameCallback = video.requestVideoFrameCallback?.bind(video);
    if (!requestVideoFrameCallback) {
      throw new Error(
        'requestVideoFrameCallback is unavailable; cannot measure whether the camera tile is advancing'
      );
    }
    const canvas = document.createElement('canvas');
    canvas.width = CAMERA_SAMPLE_WIDTH;
    canvas.height = CAMERA_SAMPLE_HEIGHT;
    const context = canvas.getContext('2d', { willReadFrequently: true });
    if (!context) {
      throw new Error(
        'a 2d canvas context is unavailable; cannot read back the camera tile pixels'
      );
    }

    // Positive control FIRST: prove this canvas can report back a colour it
    // was given, before any all-black reading off the video is allowed to
    // mean anything.
    let canvasControlOk = false;
    try {
      context.fillStyle = '#ffffff';
      context.fillRect(0, 0, canvas.width, canvas.height);
      const control = context.getImageData(0, 0, canvas.width, canvas.height).data;
      canvasControlOk = control[0] > 250 && control[1] > 250 && control[2] > 250;
    } catch {
      canvasControlOk = false;
    }

    const sampleFrame = (): { luma: Float64Array; maxLuma: number; nonBlack: number } | null => {
      // Refuse to sample a video with nothing decoded in it. `drawImage` of a
      // not-yet-ready element draws NOTHING and does not throw, so without
      // this guard the readback returns whatever was already on the canvas --
      // and what is already on the canvas is the white positive control,
      // which reads as a perfectly lit tile. A false PASS manufactured by the
      // instrument is the exact inversion of #821, and just as wrong.
      if (video.readyState < 2 || video.videoWidth <= 0 || video.videoHeight <= 0) return null;
      try {
        // Belt and braces: clear to transparent black first, so a later
        // change to the draw path cannot resurrect the stale-pixel read.
        context.clearRect(0, 0, canvas.width, canvas.height);
        context.drawImage(video, 0, 0, canvas.width, canvas.height);
        const { data } = context.getImageData(0, 0, canvas.width, canvas.height);
        const luma = new Float64Array(data.length / 4);
        let maxLuma = 0;
        let nonBlack = 0;
        for (let index = 0; index < luma.length; index += 1) {
          const offset = index * 4;
          const value =
            0.2126 * data[offset] + 0.7152 * data[offset + 1] + 0.0722 * data[offset + 2];
          luma[index] = value;
          if (value > maxLuma) maxLuma = value;
          if (value > BLACK_FLOOR) nonBlack += 1;
        }
        return { luma, maxLuma, nonBlack: nonBlack / luma.length };
      } catch {
        return null;
      }
    };

    const before = await readFramesDecoded(track);
    let frameCallbackCount = 0;
    let sampling = true;
    const onFrame = () => {
      if (!sampling) return;
      frameCallbackCount += 1;
      requestVideoFrameCallback(onFrame);
    };
    requestVideoFrameCallback(onFrame);

    // One sample early, one late: the pair is what separates a live picture
    // from a held last frame, which every counter above reports identically.
    // The early one POLLS rather than taking a single shot -- a tile that is
    // simply still coming up would otherwise leave one usable sample, which
    // the oracle (correctly) refuses to judge, turning a healthy product into
    // an INFRA-FAIL on timing alone.
    const firstSampleDeadline = Date.now() + FIRST_SAMPLE_WAIT_MS;
    let first = sampleFrame();
    while (!first && Date.now() < firstSampleDeadline) {
      await new Promise((resolve) => setTimeout(resolve, 100));
      first = sampleFrame();
    }
    await new Promise((resolve) => setTimeout(resolve, windowMs));
    const last = sampleFrame();
    sampling = false;
    const after = await readFramesDecoded(track);

    const samples = [first, last].filter(
      (sample): sample is { luma: Float64Array; maxLuma: number; nonBlack: number } =>
        sample !== null
    );
    let interFrameDiff = 0;
    if (first && last) {
      let total = 0;
      for (let index = 0; index < first.luma.length; index += 1) {
        total += Math.abs(first.luma[index] - last.luma[index]);
      }
      interFrameDiff = total / first.luma.length;
    }
    const framesDecodedDelta = before === null || after === null ? null : after - before;
    const nonBlackRatio = Math.max(...samples.map((sample) => sample.nonBlack), 0);

    const verdict = evaluateRemoteCameraTile({
      readyState: video.readyState,
      videoWidth: video.videoWidth,
      videoHeight: video.videoHeight,
      frameCallbackCount,
      framesDecodedDelta,
      windowMs,
      canvasControlOk,
      sampledFrames: samples.length,
      maxLuma: Math.round(Math.max(...samples.map((sample) => sample.maxLuma), 0)),
      nonBlackRatio,
      interFrameDiff,
    });

    return {
      ok: verdict.ok,
      classification: verdict.classification,
      fps: Number((frameCallbackCount / Math.max(windowMs / 1000, 0.001)).toFixed(2)),
      width: video.videoWidth,
      height: video.videoHeight,
      framesDecodedDelta,
      nonBlackRatio: Number(nonBlackRatio.toFixed(4)),
      interFrameDiff: Number(interFrameDiff.toFixed(3)),
      trackSid,
      publisher,
      detail: `camera from '${publisher}' track=${trackSid}: ${verdict.detail}`,
    };
  }

  async function startCockpitAudioTone(): Promise<{ trackName: string }> {
    if (!state.room) throw new Error('audio requires an active room -- join first');
    const trackName = 'petal-web-harness-tone';
    if (state.micOn && state.localAudioTrack) return { trackName };

    logEvent('cockpit: starting synthetic audio tone for AUD scenario');
    state.audioCtx = new AudioContext();
    // Headless Chrome starts an AudioContext SUSPENDED (no user gesture), and
    // a suspended context's MediaStreamDestination produces pure silence --
    // which DTX then compresses to near-zero packets. Measured 2026-08-15:
    // the native AUD oracle read 144,000 decoded samples of silence with
    // kbps=0.0 because THIS side was publishing nothing, a harness artifact
    // initially misread as #787's product defect. Resume and refuse to
    // publish unless the context is actually running -- a tone publisher that
    // cannot prove it is producing sound must fail loudly, never report
    // audioPublished: true.
    if (state.audioCtx.state !== 'running') {
      await state.audioCtx.resume().catch(() => undefined);
    }
    if (state.audioCtx.state !== 'running') {
      const detail = `AudioContext is '${state.audioCtx.state}' -- synthetic tone would be silence`;
      logEvent(`cockpit: ${detail}`, 'warn');
      throw new Error(detail);
    }
    const dest = state.audioCtx.createMediaStreamDestination();
    state.oscillator = state.audioCtx.createOscillator();
    state.oscillator.type = 'sine';
    state.oscillator.frequency.value = 440;
    const gain = state.audioCtx.createGain();
    gain.gain.value = 0.15;
    state.oscillator.connect(gain).connect(dest);
    state.oscillator.start();

    const audioTrack = dest.stream.getAudioTracks()[0];
    state.localAudioTrack = new LocalAudioTrack(audioTrack);
    try {
    // #787: RED (RFC 2198) MUST be disabled on every audio publish, mirroring the
    // native publisher's explicit `red: false` (transport/audio.rs). livekit-client
    // defaults `red: true`, and the native receiver's vendored libwebrtc accepts a
    // RED-wrapped Opus payload with zero error and then silently decodes it to
    // COMPLETE SILENCE -- measured 2026-08-14 by the cockpit AUD oracle: track
    // subscribed and active, 144,000 decoded samples, every one zero. Web->web
    // worked (both ends Chromium, both RED-capable), which is exactly the one-way
    // shape #510 documented in the other direction. Never re-enable without a
    // native decoded-PCM proof.
      await state.room.localParticipant.publishTrack(state.localAudioTrack, {
        name: trackName,
        source: Track.Source.Microphone,
        red: false,
        // The tone is a continuous test signal; DTX would let any silence bug
        // masquerade as "no traffic". Keep the packet flow observable.
        dtx: false,
      });
      state.micOn = true;
      setMicState('on (440Hz synthetic tone)', true);
      logEvent('cockpit: started publishing synthetic audio tone', 'ok');
      cb.setParticipantAudioActive(state.room.localParticipant.identity, true);
      return { trackName };
    } catch (err) {
      state.localAudioTrack = null;
      state.oscillator?.stop();
      state.oscillator = null;
      await state.audioCtx?.close();
      state.audioCtx = null;
      throw err;
    }
  }

  function installControls() {
    ctlLeave.classList.remove('danger');
    ctlLeave.classList.add('leave-subtle');

    joinBtn.addEventListener('click', () => {
      void submitMeetingField();
    });

    // Leave: full disconnect (the Disconnected handler resets state + returns
    // to the join screen).
    ctlLeave.addEventListener('click', async () => {
      await runMeetingAction('leave', async () => {
        if (state.room) {
          noteLeaveRequested();
          await state.room.disconnect();
        }
      });
    });

    // Invite: copy a click-to-join web link for this meeting. The path carries
    // an optional cosmetic label plus the short access code.
    ctlInvite.addEventListener('click', () => {
      void copyCurrentInviteLink();
    });

    roomCopyButton.addEventListener('click', () => {
      void copyCurrentInviteLink();
    });

    roomRenameButton.addEventListener('click', startRoomRename);

    // -----------------------------------------------------------------------
    // Audio control: REAL microphone via getUserMedia({audio}), published as a
    // Microphone-source track. First activation publishes; subsequent clicks
    // toggle mute/unmute via LiveKit's LocalAudioTrack.mute()/unmute() (flips
    // the underlying track + notifies the server -- no unpublish/republish).
    // The getUserMedia permission prompt cannot be clicked by unattended
    // automation -- which is exactly why the synthetic 440Hz tone in the dev
    // panel stays as a first-class dev/test option.
    // -----------------------------------------------------------------------
    ctlAudio.addEventListener('click', async () => {
      if (!state.room) return;

      if (!state.realMicOn) {
        let stream: MediaStream;
        logEvent('requesting microphone (watch for the browser mic prompt)…');
        setRealMicState('requesting microphone…', false);
        try {
          // Same hang-guard pattern as the webcam: getUserMedia can hang with
          // no prompt and no rejection when the device is held elsewhere.
          const inputDevices = await getLocalDevices('audioinput');
          const storedInputId = localStorage.getItem(HARNESS_AUDIO_INPUT_STORAGE_KEY)?.trim() ?? '';
          const preferredInputId = inputDevices.length
            ? resolvePersistedDeviceId(inputDevices, storedInputId)
            : storedInputId;
          stream = await Promise.race([
            navigator.mediaDevices.getUserMedia({ audio: audioConstraintForDeviceId(preferredInputId) }),
            new Promise<never>((_, reject) =>
              setTimeout(() => reject(new DOMException('microphone request timed out', 'TimeoutError')), 10000)
            ),
          ]);
        } catch (err) {
          const e = err as DOMException;
          if (isPermissionDeniedError(e)) permissionDenied('mic');
          else deviceChanged('mic', 'failed');
          const hint =
            e.name === 'NotAllowedError'
              ? 'permission denied — allow microphone for this site in the browser'
              : e.name === 'NotFoundError'
                ? 'no microphone found'
                : e.name === 'TimeoutError'
                  ? 'timed out — no prompt appeared (mic held by another app, or the prompt was suppressed)'
                  : `${e.name}: ${e.message}`;
          logEvent(`microphone unavailable: ${hint}`, 'error');
          setRealMicState(`unavailable — ${hint}`, false);
          showToast('Microphone unavailable');
          return;
        }
        const audioMediaTrack = stream.getAudioTracks()[0];
        const track = new LocalAudioTrack(audioMediaTrack);
        try {
          // #787: red: false -- see the tone publisher's comment above.
          await state.room.localParticipant.publishTrack(track, {
            name: MIC_TRACK_NAME,
            source: Track.Source.Microphone,
            red: false,
          });
        } catch (err) {
          showError(`Mic publish failed: ${(err as Error).message ?? err}`);
          audioMediaTrack.stop();
          return;
        }
        state.micTrack = track;
        state.realMicOn = true;
        void refreshMeetingDevices();
        cb.syncHarnessHook();
        setAudioControl('live');
        setRealMicState('on (live microphone)', true);
        logEvent('started publishing real microphone', 'ok');
        cb.setParticipantAudioActive(state.room.localParticipant.identity, true);
      } else if (state.micTrack) {
        if (state.micTrack.isMuted) {
          await state.micTrack.unmute();
          setAudioControl('live');
          setRealMicState('on (live microphone)', true);
          cb.setParticipantAudioActive(state.room.localParticipant.identity, true);
          logEvent('microphone unmuted', 'ok');
        } else {
          await state.micTrack.mute();
          setAudioControl('muted');
          setRealMicState('muted', false);
          cb.setParticipantAudioActive(state.room.localParticipant.identity, false);
          logEvent('microphone muted');
        }
      }
    });

    setupDeviceMenu(
      {
        audioCaret,
        videoCaret,
        menu: devicesMenu,
        title: devicesMenuTitle,
        body: devicesMenuBody,
      },
      {
        list: getLocalDevices,
        applyAudioInput: (deviceId) =>
          applyAudioInputDevice(deviceId).catch((err) => {
            logEvent(`microphone device switch failed: ${(err as Error).message ?? err}`, 'error');
            deviceChanged('mic', 'failed');
            showToast('Microphone switch failed');
            throw err;
          }),
        applyAudioOutput: (deviceId) =>
          applyAudioOutputDevice(deviceId).catch((err) => {
            logEvent(`speaker device switch failed: ${(err as Error).message ?? err}`, 'error');
            showToast('Speaker switch failed');
            throw err;
          }),
        applyVideoInput: (deviceId) =>
          applyVideoInputDevice(deviceId).catch((err) => {
            logEvent(`camera device switch failed: ${(err as Error).message ?? err}`, 'error');
            deviceChanged('camera', 'failed');
            showToast('Camera switch failed');
            throw err;
          }),
        storedId: (key) => {
          if (key === 'audioinput') return localStorage.getItem(HARNESS_AUDIO_INPUT_STORAGE_KEY)?.trim() ?? '';
          if (key === 'audiooutput') return localStorage.getItem(HARNESS_AUDIO_OUTPUT_STORAGE_KEY)?.trim() ?? '';
          return localStorage.getItem(HARNESS_VIDEO_INPUT_STORAGE_KEY)?.trim() ?? '';
        },
        supportsAudioOutput: supportsAudioOutputSelection,
      }
    );

    if (navigator.mediaDevices?.addEventListener) {
      navigator.mediaDevices.addEventListener('devicechange', () => {
        void refreshMeetingDevices();
      });
    }
    void refreshMeetingDevices();

    // -----------------------------------------------------------------------
    // Video control: real webcam via getUserMedia({video}), published under
    // `petal-camera-<identity-slug>` with H.264 forced (same reason as window
    // shares -- the native compositor renders petal-camera-* tracks as their
    // own borderless windows, and only H.264 decodes to the Native buffer it
    // needs).
    // -----------------------------------------------------------------------
    ctlVideo.addEventListener('click', async () => {
      if (!state.room) return;

      if (!state.webcamOn) {
        let stream: MediaStream;
        // getUserMedia can HANG indefinitely with no prompt and no rejection
        // when the camera is already held by another app (a video call / OBS /
        // another tab) or when Chrome's per-site camera permission is stuck —
        // observed live. Race it against a timeout so the UI never silently
        // wedges.
        logEvent('requesting camera (watch for the browser camera prompt)…');
        setWebcamState('requesting camera…', false);
        try {
          const videoDevices = await getLocalDevices('videoinput');
          const storedVideoId = localStorage.getItem(HARNESS_VIDEO_INPUT_STORAGE_KEY)?.trim() ?? '';
          const preferredVideoId = videoDevices.length
            ? resolvePersistedDeviceId(videoDevices, storedVideoId)
            : storedVideoId;
          stream = await Promise.race([
            navigator.mediaDevices.getUserMedia({ video: videoConstraintsForDeviceId(preferredVideoId) }),
            new Promise<never>((_, reject) =>
              setTimeout(() => reject(new DOMException('camera request timed out', 'TimeoutError')), 10000)
            ),
          ]);
        } catch (err) {
          const e = err as DOMException;
          if (isPermissionDeniedError(e)) permissionDenied('camera');
          else deviceChanged('camera', 'failed');
          const hint =
            e.name === 'NotAllowedError'
              ? 'permission denied — allow camera for this site in the browser (site-settings icon in the address bar)'
              : e.name === 'NotFoundError' || e.name === 'OverconstrainedError'
                ? 'no camera found'
                : e.name === 'NotReadableError'
                  ? 'camera is in use by another app (a call / OBS / another tab) — quit it and retry'
                  : e.name === 'TimeoutError'
                    ? 'timed out — no prompt appeared. The camera is likely held by another app, or the browser suppressed the prompt'
                    : `${e.name}: ${e.message}`;
          logEvent(`webcam unavailable: ${hint}`, 'error');
          setWebcamState(`unavailable — ${hint}`, false);
          showToast('Camera unavailable');
          return;
        }
        const camTrack = stream.getVideoTracks()[0];
        const liveCameraId = camTrack.getSettings?.().deviceId?.trim();
        if (liveCameraId) localStorage.setItem(HARNESS_VIDEO_INPUT_STORAGE_KEY, liveCameraId);
        state.localCameraTrack = new LocalVideoTrack(camTrack);
        const name = trackNameForCamera(state.room.localParticipant.identity);
        // Bounded self-heal, mirroring the desktop app's camera publish
        // (session/camera.rs `CAMERA_HEAL_RETRY_BACKOFF`): an immediate
        // attempt plus backed-off retries, then a TERMINAL user-visible
        // error. The Video button stays the working retry affordance; the
        // control is never left claiming ON while nothing publishes.
        const publishRetryDelaysMs = [2000, 4000];
        let published = false;
        let lastPublishError: unknown = null;
        for (let attempt = 0; attempt <= publishRetryDelaysMs.length; attempt += 1) {
          if (attempt > 0) {
            const delayMs = publishRetryDelaysMs[attempt - 1];
            logEvent(`webcam publish retrying in ${delayMs / 1000}s (attempt ${attempt + 1}/${publishRetryDelaysMs.length + 1})…`);
            setWebcamState('publish failed — retrying…', false);
            await new Promise((resolve) => setTimeout(resolve, delayMs));
            if (!state.room) break; // left the room mid-retry — stop trying
          }
          try {
            await state.room.localParticipant.publishTrack(state.localCameraTrack, {
              name,
              source: Track.Source.Camera,
              videoCodec: 'h264',
              videoEncoding: CAMERA_VIDEO_ENCODING,
              // Keep the real-camera path identical to the synthetic probe:
              // one 720p encoding with no SFU or encoder-resolution ramp.
              simulcast: false,
              degradationPreference: 'maintain-resolution',
            });
            published = true;
            break;
          } catch (err) {
            lastPublishError = err;
            logEvent(`webcam publish attempt ${attempt + 1} failed: ${(err as Error).message ?? err}`, 'error');
          }
        }
        if (published) {
          state.webcamOn = true;
          setVideoControl(true);
          setWebcamState(`on (${name})`, true);
          cameraTrackNameDisplay.textContent = name;
          logEvent(`started publishing webcam as "${name}"`, 'ok');
          cb.setTileCamera(state.room.localParticipant.identity, true, state.localCameraTrack, cameraWindowId(name));
          void cb.verifyH264Negotiated(state.localCameraTrack, `webcam (${name})`);
        } else {
          logEvent(`webcam publish terminally failed after retries: ${(lastPublishError as Error)?.message ?? lastPublishError}`, 'error');
          deviceChanged('camera', 'failed');
          camTrack.stop();
          state.localCameraTrack = null;
          setWebcamState('publish failed — press Video to retry', false);
          showToast('Camera publish failed — press Video to retry');
        }
      } else {
        if (state.localCameraTrack) {
          await state.room.localParticipant.unpublishTrack(state.localCameraTrack, true);
          state.localCameraTrack.mediaStreamTrack.stop();
          state.localCameraTrack = null;
        }
        if (state.syntheticCameraIntervalId !== null) {
          clearInterval(state.syntheticCameraIntervalId);
          state.syntheticCameraIntervalId = null;
        }
        state.webcamOn = false;
        setVideoControl(false);
        setWebcamState('off', false);
        logEvent('stopped publishing webcam');
        if (state.room) cb.clearTileCamera(state.room.localParticipant.identity);
      }
    });

    // -----------------------------------------------------------------------
    // Screensharing control: REAL screen share via getDisplayMedia (the
    // browser shows its own window/tab/screen picker), published under a fresh
    // petal-window-<random u32> track name with H.264 forced. It is meant to
    // match the test pattern and the native app's publish contract
    // (transport/publisher.rs) -- and for a long time silently did NOT: this
    // path alone set no explicit encoding and inherited livekit's 2.5Mbps/
    // 15fps default, which is what shipped a 2560x1600 desktop as 320x180.
    // `screenSharePublishEncoding` + its contract tests now pin the parity
    // this comment claims. NOTE: the getDisplayMedia picker cannot be
    // clicked by unattended automation -- that is exactly why the synthetic
    // test-pattern share in the dev panel is kept as a first-class option.
    // -----------------------------------------------------------------------
    ctlShare.addEventListener('click', async () => {
      if (!state.room) return;

      if (!state.screenSharing) {
        let stream: MediaStream;
        feedbackReport?.onShareStartIntent();
        logEvent('requesting screen capture (watch for the browser picker)…');
        try {
          stream = await navigator.mediaDevices.getDisplayMedia({
            video: {
              width: { ideal: 2560 },
              height: { ideal: 1600 },
              frameRate: { ideal: 60, max: 60 },
            },
            audio: false,
          });
        } catch (err) {
          const e = err as DOMException;
          const hint =
            e.name === 'NotAllowedError' ? 'picker dismissed / permission denied' : `${e.name}: ${e.message}`;
          logEvent(`screen share unavailable: ${hint}`, 'error');
          showToast('Screen share cancelled');
          feedbackReport?.onShareEnded();
          return;
        }
        const mediaTrack = stream.getVideoTracks()[0];
        mediaTrack.contentHint = 'detail';
        const windowId = randomWindowId();
        const track = new LocalVideoTrack(mediaTrack);
        // Size the encoding from what we actually captured, not from what we
        // asked for -- the browser/user picks the real source, and a display
        // is often far larger than the `ideal` above.
        const captured = mediaTrack.getSettings();
        const shareEncoding = screenSharePublishEncoding(captured.width, captured.height);
        logEvent(
          `screen share encoding: ${captured.width ?? '?'}x${captured.height ?? '?'} -> ` +
            `${Math.round(shareEncoding.maxBitrate / 1_000_000)}Mbps @${shareEncoding.maxFramerate}fps`
        );
        try {
          await state.room.localParticipant.publishTrack(track, {
            name: trackNameForWindow(windowId),
            source: Track.Source.ScreenShare,
            // H.264 is load-bearing -- see verifyH264Negotiated's doc comment.
            videoCodec: 'h264',
            // Without these the default preset (2.5Mbps/15fps) starves a
            // desktop-sized capture and the encoder sheds RESOLUTION to cope --
            // a 2560x1600 share reached a receiver as 320x180. 'maintain-
            // resolution' is what keeps text legible: drop frames, not pixels.
            screenShareEncoding: shareEncoding,
            degradationPreference: 'maintain-resolution',
            frameMetadata: { timestamp: true, frameId: true },
          });
          await setLocalParticipantMetadata(
            state.room.localParticipant,
            mergeSharedSourceMetadata(state.room.localParticipant.metadata, windowId, 'display', {
              // A browser cannot inject OS input: advertise this share as
              // NOT controllable so native receivers hide the affordance
              // instead of offering a button that always times out.
              remoteControllable: false,
            })
          );
        } catch (err) {
          showError(`Screen share publish failed: ${(err as Error).message ?? err}`);
          mediaTrack.stop();
          feedbackReport?.onShareEnded();
          return;
        }
        state.screenTrack = track;
        state.screenWindowId = windowId;
        state.screenSharing = true;
        cb.syncHarnessHook();
        // The browser's own "Stop sharing" bar / tab close ends the capture
        // track outside our UI -- unpublish cleanly when that happens.
        mediaTrack.addEventListener('ended', () => {
          void stopScreenShare('browser');
        });
        setShareControl(
          true,
          state.room.localParticipant.identity,
          identityPaletteIndexFromMetadata(state.room.localParticipant.metadata)
        );
        setScreenShareState(`sharing (window_id=${windowId})`, true);
        logEvent(`started REAL screen share as "${trackNameForWindow(windowId)}"`, 'ok');
        shareStarted('picker');
        cb.addShareTile(
          state.room.localParticipant.identity,
          true,
          'screen',
          track,
          participantDisplayName(state.room.localParticipant.identity, displayNameInput.value),
          windowId
        );
        void cb.verifyH264Negotiated(track, `screen share (window_id=${windowId})`);
      } else {
        await stopScreenShare('button');
      }
    });

    // -----------------------------------------------------------------------
    // Dev panel: synthetic test-pattern share (canvas.captureStream(30) ->
    // publish under Petal's track_name_for_window format). Kept as a
    // first-class option because unattended automation cannot click
    // getDisplayMedia's picker. `startTestPatternShare` is extracted (rather
    // than living inline in the click listener) so the test-cockpit
    // `__petalHarness.cockpitAutoScenario.sharePattern()` automation hook
    // (#254) can drive the
    // exact same publish path headlessly, with no synthetic click.
    // -----------------------------------------------------------------------
    shareBtn.addEventListener('click', async () => {
      if (!state.room) return;

      if (!state.sharing) {
        await startTestPatternShare();
      } else {
        cb.stopTelepointerSender();
        try {
          if (state.localVideoTrack) await state.room.localParticipant.unpublishTrack(state.localVideoTrack, true);
        } finally {
          state.localVideoTrack = null;
          state.sharing = false;
          removePresentationSourceHost();
        }
        try {
          await setLocalParticipantMetadata(
            state.room.localParticipant,
            mergeSharedSourceMetadata(state.room.localParticipant.metadata, ctx.windowId, null)
          );
        } catch {
          // best-effort: the publication is already gone, a stale scale entry
          // only lingers until the next merge
        }
        feedbackReport?.onShareEnded();
        cb.syncHarnessHook();
        setShareState('not sharing', false);
        shareBtn.textContent = 'Share test pattern';
        logEvent('stopped sharing test pattern');
        shareStopped('user');
        if (state.room) cb.removeShareTile(state.room.localParticipant.identity, 'testpattern');
      }
    });

    // -----------------------------------------------------------------------
    // Dev panel: synthetic oscillator tone captured as a MediaStreamTrack via
    // the Web Audio API, NOT a real microphone track.
    //
    // Decision + rationale (see web-harness/README.md for the full writeup):
    // real `getUserMedia({ audio: true })` mic capture requires an OS-level
    // permission prompt that browser-automation tooling cannot reliably click
    // through headlessly, and would leave the automated "is the pipeline
    // healthy" signal dependent on host mic hardware/permissions being present
    // at all. A Web Audio OscillatorNode -> MediaStreamAudioDestinationNode
    // produces a real, continuous MediaStreamTrack (same shape publishTrack
    // expects) with zero permission dependency and a deterministic,
    // easy-to-verify 440Hz tone -- reliably drivable by automation, which is
    // the property the dev/test path needs most. The REAL mic lives on the
    // Audio control button above.
    // -----------------------------------------------------------------------
    micCheckbox.addEventListener('change', async () => {
      if (!state.room) return;

      if (micCheckbox.checked && !state.micOn) {
        try {
          state.audioCtx = new AudioContext();
          const dest = state.audioCtx.createMediaStreamDestination();
          state.oscillator = state.audioCtx.createOscillator();
          state.oscillator.type = 'sine';
          state.oscillator.frequency.value = 440;
          const gain = state.audioCtx.createGain();
          gain.gain.value = 0.15; // quiet -- this only needs to prove a live audio track exists
          state.oscillator.connect(gain).connect(dest);
          state.oscillator.start();
          // Suspended-context guard -- see startCockpitAudioTone.
          if (state.audioCtx.state !== 'running') await state.audioCtx.resume().catch(() => undefined);

          const audioTrack = dest.stream.getAudioTracks()[0];
          state.localAudioTrack = new LocalAudioTrack(audioTrack);
          // #787: red: false -- see the AUD tone publisher's comment above.
          await state.room.localParticipant.publishTrack(state.localAudioTrack, {
            name: 'petal-web-harness-tone',
            source: Track.Source.Microphone,
            red: false,
          });
          state.micOn = true;
          setMicState('on (440Hz synthetic tone)', true);
          logEvent('started publishing synthetic audio tone', 'ok');
          cb.setParticipantAudioActive(state.room.localParticipant.identity, true);
        } catch (err) {
          showError(`Tone publish failed: ${(err as Error).message ?? err}`);
          micCheckbox.checked = false;
        }
      } else if (!micCheckbox.checked && state.micOn) {
        if (state.localAudioTrack) {
          await state.room.localParticipant.unpublishTrack(state.localAudioTrack, true);
          state.localAudioTrack = null;
        }
        state.oscillator?.stop();
        state.oscillator = null;
        await state.audioCtx?.close();
        state.audioCtx = null;
        state.micOn = false;
        setMicState('off', false);
        logEvent('stopped publishing audio tone');
        if (state.room && !state.realMicOn) cb.setParticipantAudioActive(state.room.localParticipant.identity, false);
      }
    });

    // -----------------------------------------------------------------------
    // Refs #378: local echo -- opt-in, default OFF. Purely a local-rendering
    // toggle for remoteControlUi.ts's controller overlay (no wire message,
    // no Rust/native counterpart), so this only flips state + persists it,
    // unlike the tone toggle above which publishes a real track.
    // -----------------------------------------------------------------------
    localEchoCheckbox.addEventListener('change', () => {
      state.localEchoEnabled = localEchoCheckbox.checked;
      if (localEchoCheckbox.checked) localStorage.setItem(HARNESS_LOCAL_ECHO_STORAGE_KEY, '1');
      else localStorage.removeItem(HARNESS_LOCAL_ECHO_STORAGE_KEY);
      logEvent(`local echo ${localEchoCheckbox.checked ? 'enabled' : 'disabled'} (experimental, #378)`);
    });

    // -----------------------------------------------------------------------
    // #669: Debug mode -- opt-in, default OFF, gates the remote-window
    // header's Debug button (`debugHeaderControlVisible`, shared with the
    // desktop client). `syncRemoteWindowHeaders` re-runs every live header's
    // `syncMode()` so an already-open remote window picks up the toggle
    // immediately, matching the desktop client's live-propagation behavior
    // (there via a Rust `emit`; here it's simpler since this is one JS realm).
    // -----------------------------------------------------------------------
    debugModeCheckbox.addEventListener('change', () => {
      state.debugModeEnabled = debugModeCheckbox.checked;
      if (debugModeCheckbox.checked) localStorage.setItem(HARNESS_DEBUG_MODE_STORAGE_KEY, '1');
      else localStorage.removeItem(HARNESS_DEBUG_MODE_STORAGE_KEY);
      logEvent(`debug mode ${debugModeCheckbox.checked ? 'enabled' : 'disabled'} (#669)`);
      cb.syncRemoteWindowHeaders();
    });
  }

  async function stopScreenShare(reason: 'button' | 'browser') {
    if (!state.screenSharing) return;
    const track = state.screenTrack;
    state.screenTrack = null;
    state.screenSharing = false;
    feedbackReport?.onShareEnded();
    const endedId = state.screenWindowId;
    state.screenWindowId = null;
    cb.syncHarnessHook();
    if (track) {
      if (state.room) {
        try {
          await state.room.localParticipant.unpublishTrack(track, true);
        } catch {
          // room may already be tearing down
        }
        cb.removeShareTile(state.room.localParticipant.identity, 'screen');
      }
      track.mediaStreamTrack.stop();
    }
    if (state.room && endedId !== null) {
      await setLocalParticipantMetadata(
        state.room.localParticipant,
        mergeSharedSourceMetadata(state.room.localParticipant.metadata, endedId, null)
      ).catch(() => {});
    }
    setShareControl(false);
    setScreenShareState('not sharing', false);
    logEvent(
      reason === 'browser'
        ? `screen share ended by the browser's own stop-sharing UI (window_id=${endedId})`
        : `stopped screen share (window_id=${endedId})`
    );
    shareStopped('user');
  }

  return {
    installControls,
    resolveIdentity,
    submitMeetingField,
    renameRoomDisplayName,
    startTestPatternShare,
    startCockpitWebcam,
    stopCockpitWebcam,
    startCockpitAudioTone,
    measureCockpitRemoteAudio,
    measureCockpitRemoteCamera,
  };
}

async function setLocalParticipantMetadata(participant: unknown, metadata: string): Promise<void> {
  const setter = (participant as { setMetadata?: (metadata: string) => Promise<void> }).setMetadata;
  if (typeof setter === 'function') {
    await setter.call(participant, metadata);
  }
}
