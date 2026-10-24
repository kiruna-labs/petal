import type { RemoteControlStatus } from './ipc';

export const REMOTE_CONTROL_REQUEST_TIMEOUT_MS = 8000;
/** Once the host reports `awaitingConsent` the sharer has 30 s to answer
 * (host-side `CONSENT_TIMEOUT`); the controller's own timeout is extended to
 * cover that window plus transit so a well-behaved consent flow never trips
 * the 8 s no-answer timeout first. */
export const REMOTE_CONTROL_CONSENT_TIMEOUT_MS = 35000;
export const REMOTE_CONTROL_TIMEOUT_MESSAGE =
  'Remote control request timed out. Check Accessibility on the shared Mac, then try again.';
export const REMOTE_CONTROL_CONSENT_TIMEOUT_MESSAGE =
  'The sharer did not respond to the control request.';

export type RemoteControlFeedbackStatus = RemoteControlStatus['status'] | null;
export type RemoteControlStatusEffect = 'activate' | 'feedback' | 'terminate';

export function remoteControlStatusEffect(
  status: RemoteControlStatus['status']
): RemoteControlStatusEffect {
  if (status === 'active') return 'activate';
  if (status === 'stopped' || status === 'disabled') return 'terminate';
  return 'feedback';
}

/** Statuses rendered with the neutral (non-warning) treatment: they are
 * structural "not now" answers, not a failure of anything the controller did. */
export function remoteControlFeedbackIsNeutral(status: RemoteControlFeedbackStatus): boolean {
  return status === 'requestUnavailable' || status === 'awaitingConsent';
}

export function remoteControlFeedbackLabel(status: RemoteControlFeedbackStatus): string | null {
  switch (status) {
    case 'awaitingConsent':
      return 'Waiting for approval';
    case 'denied':
      return 'Control denied';
    case 'accessibilityDenied':
      return 'Needs access';
    case 'requestFailed':
      return 'Input ignored';
    case 'disabled':
      return 'Disabled';
    case 'targetPaused':
      return 'Paused';
    case 'targetUnavailable':
      // Control is on, but this input did not reach the shared window.
      return 'Window out of reach';
    case 'requestUnavailable':
      // Control cannot be started for this share at all.
      return 'Control unavailable';
    case 'textTruncated':
      return 'Text capped';
    case 'notForeground':
      return 'Not foreground';
    case 'occluded':
      return 'Covered';
    case 'integrityBlocked':
      return 'Blocked';
    case 'secureField':
      return 'Secure field';
    case 'unsupportedRoute':
      return 'Unsupported';
    case 'staleShareInstance':
      return 'Share changed';
    case 'injectionTimeout':
      return 'Timed out';
    default:
      return status ? 'Input ignored' : null;
  }
}

/** Plain-language copy, with the next step, for statuses whose native message
 * is engineering text ("input could not be delivered to the shared window").
 * The sharer is the only one who can fix these, so the copy says what to ask. */
const REMOTE_CONTROL_FEEDBACK_DETAIL: Partial<Record<RemoteControlStatus['status'], string>> = {
  targetUnavailable:
    "Your input didn't reach the shared window. Ask the sharer to bring it to the front; it may be minimized or on another desktop.",
  notForeground: 'Keyboard needs the shared app in front. Ask the sharer to click into it.',
  occluded: 'Another window is covering the shared window. Ask the sharer to bring it to the front.'
};

export function remoteControlFeedbackTitle(
  status: RemoteControlFeedbackStatus,
  message: string | null
): string | null {
  if (!status || status === 'active' || status === 'stopped') return null;
  return REMOTE_CONTROL_FEEDBACK_DETAIL[status] ?? (message || 'Remote control is not available right now.');
}

/** Below this width the remote window header collapses its status chip to an
 * icon-only dot (RemoteWindowHeader.svelte `@container (max-width: 720px)`).
 * The control overlay spans the same width as the header. */
export const HEADER_STATUS_TEXT_MIN_WIDTH = 720;

/** Whether the control overlay's feedback banner should repeat the status
 * label. When the header chip already shows it in text, the banner shows only
 * the detail (what happened and what to do), so the status is never doubled. */
export function controlBannerShowsLabel(overlayWidth: number): boolean {
  return !(overlayWidth > HEADER_STATUS_TEXT_MIN_WIDTH);
}
