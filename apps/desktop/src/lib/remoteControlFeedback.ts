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
    case 'requestUnavailable':
      return 'Unavailable';
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

export function remoteControlFeedbackTitle(
  status: RemoteControlFeedbackStatus,
  message: string | null
): string | null {
  if (!status || status === 'active' || status === 'stopped') return null;
  return message || 'Remote control is not available right now.';
}
