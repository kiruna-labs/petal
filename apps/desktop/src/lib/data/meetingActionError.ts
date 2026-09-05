export type MeetingAction = 'create' | 'join' | 'leave' | 'open';

const FALLBACK_BY_ACTION: Record<MeetingAction, string> = {
  create: 'Could not create the meeting.',
  join: 'Could not join the meeting.',
  leave: 'Could not leave the meeting.',
  open: 'Meeting was created, but Petal could not open it.',
};

export function meetingActionErrorMessage(error: unknown, action: MeetingAction): string {
  const fallback = FALLBACK_BY_ACTION[action];
  const detail = errorMessage(error);

  if (!detail) return `${fallback} Check your connection and try again.`;

  if (/failed to fetch|network|dns|timeout|timed out|backend|token|server|unreachable/i.test(detail)) {
    return `${fallback} Petal could not reach the meeting server. Check your connection and try again.`;
  }

  if (/invalid|malformed|unrecognized|credential|invite|meeting code|access code/i.test(detail)) {
    return `${fallback} Check the invite link or meeting code and try again.`;
  }

  if (/not found|missing|gone|ended|closed|forbidden|denied|unauthor/i.test(detail)) {
    return `${fallback} This meeting may have ended, or you may not have access.`;
  }

  if (/route|navigation|goto|open/i.test(detail)) {
    return `${fallback} Try selecting it from Your Rooms.`;
  }

  return `${fallback} Check your connection and try again.`;
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message.trim();
  if (typeof error === 'string') return error.trim();
  return '';
}
