export const INVITE_LINK_COPIED_LABEL = 'Invite link copied to clipboard:';

export function inviteLinkCopiedToastMessage(url: string): string {
  return `${INVITE_LINK_COPIED_LABEL}\n${url}`;
}
