import {
  generateAccessCode as defaultGenerateAccessCode,
  accessCodeFromInviteInput,
  looksLikeMeetingCredentialInput,
  meetingCredentialFromInviteInput
} from './meetingCode.ts';
import { meetingActionErrorMessage, type MeetingAction } from './meetingActionError.ts';

type MainMenuMeetingAction = Extract<MeetingAction, 'create' | 'join'>;

export type MainMenuMeetingActionResult = {
  action: MainMenuMeetingAction | null;
  error: string | null;
  nextInput: string;
  submitted: boolean;
};

export type MainMenuMeetingActionCallbacks = {
  onCreateMeeting?: (name: string, displayName: string | null) => void | Promise<void>;
  onJoinByCode?: (name: string, accessCode: string) => void | Promise<void>;
};

export const INVALID_MEETING_INPUT_ERROR = 'Paste a full invite link or meeting code.';

function looksLikeInviteLinkOrCode(value: string): boolean {
  return (
    looksLikeMeetingCredentialInput(value) ||
    /^[a-z][a-z0-9+.-]*:\/\//i.test(value) ||
    /^\/?j\//i.test(value) ||
    /[?#&]code=/i.test(value) ||
    /#\/join\//i.test(value)
  );
}

async function runAction(
  action: MainMenuMeetingAction,
  nextInput: string,
  task: () => void | Promise<void>
): Promise<MainMenuMeetingActionResult> {
  try {
    await task();
    return { action, error: null, nextInput, submitted: true };
  } catch (error) {
    return {
      action,
      error: meetingActionErrorMessage(error, action),
      nextInput,
      submitted: false
    };
  }
}

export async function submitMainMenuMeetingAction(
  input: string,
  callbacks: MainMenuMeetingActionCallbacks,
  generateAccessCode: () => string = defaultGenerateAccessCode
): Promise<MainMenuMeetingActionResult> {
  const trimmed = input.trim();

  if (!trimmed) {
    if (!callbacks.onCreateMeeting) return { action: null, error: null, nextInput: input, submitted: false };
    // Blank create passes an access code. Rust derives the hidden room
    // credential from it so invited peers typing the code reach the same room.
    const accessCode = generateAccessCode();
    return runAction('create', input, () => callbacks.onCreateMeeting?.(accessCode, null));
  }

  const credential = meetingCredentialFromInviteInput(trimmed);
  if (credential) {
    if (!callbacks.onJoinByCode) return { action: null, error: null, nextInput: input, submitted: false };
    const accessCode = accessCodeFromInviteInput(trimmed);
    if (!accessCode) return { action: null, error: INVALID_MEETING_INPUT_ERROR, nextInput: input, submitted: false };
    return runAction('join', '', () => callbacks.onJoinByCode?.(credential, accessCode));
  }

  if (looksLikeInviteLinkOrCode(trimmed)) {
    return { action: null, error: INVALID_MEETING_INPUT_ERROR, nextInput: input, submitted: false };
  }

  if (!callbacks.onCreateMeeting) return { action: null, error: null, nextInput: input, submitted: false };

  const displayName = trimmed;
  // Typed names are display labels only; joining/auth still uses a generated
  // access-code credential instead of treating public names as room secrets.
  const accessCode = generateAccessCode();
  return runAction('create', '', () => callbacks.onCreateMeeting?.(accessCode, displayName));
}
