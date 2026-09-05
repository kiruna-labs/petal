import { accessCodeForCredential, normalizeAccessCode } from '@petal/shared/logic/meetingCode';

/** Return only the public access code; an opaque room credential is never UI copy. */
export function publicInviteAccessCode(value: string | null | undefined): string | null {
  if (!value) return null;
  return normalizeAccessCode(value) ?? accessCodeForCredential(value);
}

export function inviteCopyAriaLabel(value: string | null | undefined): string {
  const accessCode = publicInviteAccessCode(value);
  return accessCode ? `Room ID ${accessCode}, click to copy invite` : 'Copy invite link';
}

export function inviteCopyTooltip(value: string | null | undefined): string {
  const accessCode = publicInviteAccessCode(value);
  return accessCode ? `Room ID: ${accessCode} (click to copy invite)` : 'Copy invite link';
}
