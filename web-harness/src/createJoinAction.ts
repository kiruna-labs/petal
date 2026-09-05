import { INVALID_JOIN_INPUT_ERROR, looksLikeJoinAttempt, parseJoinInput } from '@petal/shared/logic/joinInput';
import { meetingActionErrorMessage, type MeetingAction } from './meetingActionError.ts';
import { generateMeetingCode } from '@petal/shared/logic/meetingCode';

export type MeetingActionFeedback = {
  logEvent: (message: string, level: 'error') => void;
  showError: (message: string) => void;
  showToast: (message: string) => void;
};

export function newMeetingCredentialFromInput(
  value: string,
  generateCredential: (label?: string) => string = generateMeetingCode
): { code: string; displayName: string | null } {
  const displayName = value.trim();
  return {
    code: generateCredential(displayName || undefined),
    displayName: displayName || null,
  };
}

export async function runWebMeetingAction(
  action: MeetingAction,
  task: () => Promise<void>,
  feedback: MeetingActionFeedback
): Promise<boolean> {
  try {
    await task();
    return true;
  } catch (err) {
    const message = meetingActionErrorMessage(err, action);
    feedback.showError(message);
    feedback.showToast(message);
    feedback.logEvent(`${action} meeting failed: ${(err as Error)?.message ?? err}`, 'error');
    return false;
  }
}

export type SubmitWebCreateJoinOptions = {
  clearError: () => void;
  connectWithCredential: (code: string) => Promise<void>;
  credentialForNewMeeting?: (label?: string) => string;
  logEvent: (message: string, level: 'error') => void;
  pendingRecentCredential: () => string | null;
  rawInput: string;
  renameRoomDisplayName: (code: string, displayName: string | null) => string;
  showError: (message: string) => void;
  showToast: (message: string) => void;
};

export async function submitWebCreateJoinAction({
  clearError,
  connectWithCredential,
  credentialForNewMeeting = generateMeetingCode,
  logEvent,
  pendingRecentCredential,
  rawInput,
  renameRoomDisplayName,
  showError,
  showToast
}: SubmitWebCreateJoinOptions): Promise<void> {
  clearError();
  const recentCredential = pendingRecentCredential();
  if (recentCredential) {
    await runWebMeetingAction('join', () => connectWithCredential(recentCredential), {
      logEvent,
      showError,
      showToast
    });
    return;
  }

  if (!rawInput.trim()) {
    const code = credentialForNewMeeting();
    await runWebMeetingAction('create', () => connectWithCredential(code), {
      logEvent,
      showError,
      showToast
    });
    return;
  }

  const parsed = parseJoinInput(rawInput);
  if (parsed.ok) {
    await runWebMeetingAction('join', () => connectWithCredential(parsed.code), {
      logEvent,
      showError,
      showToast
    });
    return;
  }

  if (looksLikeJoinAttempt(rawInput)) {
    showError(INVALID_JOIN_INPUT_ERROR);
    return;
  }

  const { code, displayName } = newMeetingCredentialFromInput(rawInput, credentialForNewMeeting);
  if (displayName) {
    renameRoomDisplayName(code, displayName);
  }
  await runWebMeetingAction('create', () => connectWithCredential(code), {
    logEvent,
    showError,
    showToast
  });
}
