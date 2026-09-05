import type { LogKind } from './logging';
import { inviteCopyAriaLabel, inviteCopyTooltip } from '../inviteCopy.ts';
import { roomDisplayLabelForCredentialWithDisplayName } from '../roomLabels.ts';
import { colorForIdentity, inkForIdentity } from '../telepointer.ts';
import { showSharedToast, type SharedToastAction } from '../toastMount.ts';

interface UiHelperOptions {
  joinScreen: HTMLDivElement;
  meetingScreen: HTMLDivElement;
  connectingScreen: HTMLDivElement | null;
  connectingTitle: HTMLElement | null;
  connectingStatus: HTMLElement | null;
  displayNameInput: HTMLInputElement;
  meetingCodeInput: HTMLInputElement;
  joinBtn: HTMLButtonElement;
  createBtn: HTMLButtonElement | null;
  connError: HTMLElement;
  roomNameEl: HTMLElement;
  roomCopyButton: HTMLButtonElement;
  elapsedEl: HTMLElement;
  connState: HTMLElement;
  shareState: HTMLElement;
  shareScreenState: HTMLElement;
  micRealState: HTMLElement;
  micState: HTMLElement;
  webcamState: HTMLElement;
  toastEl: HTMLDivElement;
  ctlAudio: HTMLButtonElement;
  ctlVideo: HTMLButtonElement;
  ctlShare: HTMLButtonElement;
  ctlShareLabel: HTMLElement;
  ctlInvite: HTMLButtonElement;
  ctlInviteTooltip: HTMLElement;
  updateUnifiedCtaLabel: () => void;
  logEvent: (message: string, kind?: LogKind) => void;
}

export function setupUiHelpers(options: UiHelperOptions) {
  let elapsedTimer: ReturnType<typeof setInterval> | null = null;
  let connectedAt: number | null = null;

  function setConnState(text: string, cls: 'idle' | 'connecting' | 'connected' | 'error') {
    options.connState.textContent = text;
    options.connState.className = `conn-chip state-${cls}`;
  }

  function showError(message: string) {
    options.connError.textContent = message;
    options.connError.classList.remove('hidden');
    options.logEvent(message, 'error');
  }

  function clearError() {
    options.connError.textContent = '';
    options.connError.classList.add('hidden');
  }

  function formatElapsed(ms: number): string {
    const total = Math.floor(ms / 1000);
    const m = Math.floor(total / 60);
    const s = total % 60;
    const mm = m >= 100 ? String(m) : String(m).padStart(2, '0');
    return `${mm}:${String(s).padStart(2, '0')}`;
  }

  function setInviteCopyControls(code: string | null) {
    const ariaLabel = inviteCopyAriaLabel(code);
    const tooltip = inviteCopyTooltip(code);
    for (const control of [options.roomCopyButton, options.ctlInvite]) {
      control.setAttribute('aria-label', ariaLabel);
      control.title = tooltip;
    }
    options.ctlInviteTooltip.textContent = tooltip;
  }

  function hideConnectingScreen() {
    options.connectingScreen?.classList.add('hidden');
  }

  /** Join-link auto-join path (#deepLink): swap the home screen for a
   * "Joining <label>" card immediately, so the menu never flashes before the
   * meeting appears. Every exit -- success (`showMeetingScreen`) or failure
   * (`showJoinScreen` via `resetFailedJoinUi`) -- dismisses it. */
  function showConnectingScreen(label: string) {
    if (!options.connectingScreen) return;
    if (options.connectingTitle) options.connectingTitle.textContent = `Joining ${label}`;
    if (options.connectingStatus) options.connectingStatus.textContent = 'Connecting…';
    options.joinScreen.classList.add('hidden');
    options.meetingScreen.classList.add('hidden');
    options.connectingScreen.classList.remove('hidden');
  }

  function setConnectingStatus(text: string) {
    if (options.connectingStatus) options.connectingStatus.textContent = text;
  }

  function showMeetingScreen(code: string, roomDisplayName?: string | null) {
    hideConnectingScreen();
    options.joinScreen.classList.add('hidden');
    options.meetingScreen.classList.remove('hidden');
    options.roomNameEl.textContent = roomDisplayLabelForCredentialWithDisplayName(code, roomDisplayName);
    setInviteCopyControls(code);
    connectedAt = Date.now();
    options.elapsedEl.textContent = '00:00';
    if (elapsedTimer === null) {
      elapsedTimer = setInterval(() => {
        if (connectedAt !== null) options.elapsedEl.textContent = formatElapsed(Date.now() - connectedAt);
      }, 1000);
    }
  }

  function showJoinScreen() {
    hideConnectingScreen();
    options.meetingScreen.classList.add('hidden');
    options.joinScreen.classList.remove('hidden');
    if (elapsedTimer !== null) {
      clearInterval(elapsedTimer);
      elapsedTimer = null;
    }
    connectedAt = null;
    setInviteCopyControls(null);
  }

  function showToast(message: string) {
    // Rendered through the SHARED Svelte Toast component (toastMount.ts) —
    // the same pill the desktop app uses. The #toast wrapper keeps only the
    // positioning/wrapping CSS; the shared component owns the visuals.
    showSharedToast(options.toastEl, message);
  }

  function showActionableToast(message: string, dismissMs: number, action?: SharedToastAction) {
    showSharedToast(options.toastEl, message, dismissMs, action);
  }

  function setShareState(text: string, on: boolean) {
    options.shareState.textContent = text;
    options.shareState.className = `state ${on ? 'state-on' : 'state-idle'}`;
  }

  function setMicState(text: string, on: boolean) {
    options.micState.textContent = text;
    options.micState.className = `state ${on ? 'state-on' : 'state-idle'}`;
  }

  function setScreenShareState(text: string, on: boolean) {
    options.shareScreenState.textContent = text;
    options.shareScreenState.className = `state ${on ? 'state-on' : 'state-idle'}`;
  }

  function setRealMicState(text: string, on: boolean) {
    options.micRealState.textContent = text;
    options.micRealState.className = `state ${on ? 'state-on' : 'state-idle'}`;
  }

  function setWebcamState(text: string, on: boolean) {
    options.webcamState.textContent = text;
    options.webcamState.className = `state ${on ? 'state-on' : 'state-idle'}`;
  }

  function setJoinControlsEnabled(enabled: boolean) {
    options.joinBtn.disabled = !enabled;
    if (options.createBtn) options.createBtn.disabled = !enabled;
    options.meetingCodeInput.disabled = !enabled;
    options.displayNameInput.disabled = !enabled;
    options.updateUnifiedCtaLabel();
  }

  function setAudioControl(state: 'off' | 'live' | 'muted') {
    options.ctlAudio.classList.toggle('danger', state !== 'live');
    options.ctlAudio.classList.toggle('slashed', state !== 'live');
    options.ctlAudio.setAttribute('aria-pressed', state === 'live' ? 'true' : 'false');
    options.ctlAudio.setAttribute(
      'aria-label',
      state === 'off' ? 'Enable microphone' : state === 'live' ? 'Mute microphone' : 'Unmute microphone'
    );
  }

  function setVideoControl(on: boolean) {
    options.ctlVideo.classList.toggle('slashed', !on);
    options.ctlVideo.setAttribute('aria-pressed', on ? 'true' : 'false');
    options.ctlVideo.setAttribute('aria-label', on ? 'Stop camera' : 'Start camera');
  }

  function setShareControl(on: boolean, identity?: string | null, paletteIndex?: number | null) {
    const trimmedIdentity = identity?.trim();
    if (on && trimmedIdentity) {
      options.ctlShare.style.setProperty('--control-live-bg', colorForIdentity(trimmedIdentity, paletteIndex));
      options.ctlShare.style.setProperty('--control-live-fg', inkForIdentity(trimmedIdentity, paletteIndex));
    } else if (!on) {
      options.ctlShare.style.removeProperty('--control-live-bg');
      options.ctlShare.style.removeProperty('--control-live-fg');
    }
    options.ctlShare.classList.toggle('live', on);
    options.ctlShareLabel.classList.toggle('on', on);
    options.ctlShare.setAttribute('aria-pressed', on ? 'true' : 'false');
    options.ctlShare.setAttribute('aria-label', on ? 'Stop sharing your screen' : 'Share your screen');
  }

  setAudioControl('off');
  setVideoControl(false);
  setShareControl(false);

  return {
    setConnState,
    showError,
    clearError,
    showMeetingScreen,
    showJoinScreen,
    showConnectingScreen,
    setConnectingStatus,
    showToast,
    showActionableToast,
    setShareState,
    setMicState,
    setScreenShareState,
    setRealMicState,
    setWebcamState,
    setJoinControlsEnabled,
    setAudioControl,
    setVideoControl,
    setShareControl,
  };
}
