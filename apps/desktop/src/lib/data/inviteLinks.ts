import {
  accessCodeForCredential,
  buildMeetingInvitePath,
  meetingDisplayLabelFromCredential,
  normalizeAccessCode
} from './meetingCode.ts';
import type { RoomRecord } from '$lib/ipc';

// Origin for copied invite links. A self-hosted deployment overrides it at
// build time with `VITE_PETAL_INVITE_ORIGIN` (docs/SELF_HOSTING.md); the
// official value is the default so official builds need no extra env.
export const INVITE_ORIGIN =
  (import.meta.env?.VITE_PETAL_INVITE_ORIGIN as string | undefined)?.trim().replace(/\/$/, '') ||
  'https://meet.petal.live';
export const INVITE_LINK_COPIED_LABEL = 'Invite link copied to clipboard:';

/**
 * Copy controls may receive a short access code while the meeting is joining,
 * or its opaque credential once it has joined. Never let the latter leak into
 * UI copy: this helper returns a canonical public code, if one is available.
 */
export function publicInviteAccessCode(value: string | null | undefined): string | null {
  if (!value) return null;
  return normalizeAccessCode(value) ?? accessCodeForCredential(value);
}

/** The saved room a route param, `current_room`, or presence event names:
 * its durable credential (`name`) or its `slug`. */
export function findRoomRecord<T extends Pick<RoomRecord, 'name' | 'slug'>>(
  rooms: readonly T[],
  nameOrSlug: string | null | undefined
): T | null {
  if (!nameOrSlug) return null;
  return rooms.find((room) => room.name === nameOrSlug || room.slug === nameOrSlug) ?? null;
}

/**
 * The public code behind the active meeting's invite controls. The joined
 * record is authoritative, but `join_room` resolves only after the WHOLE join
 * -- audio preparation alone can hold it 45s (#787) -- so until then the saved
 * record from rooms.json supplies the code. The route param is the last
 * resort: the in-memory credential map only knows codes this webview generated
 * or parsed itself, never one the native side created or stored.
 */
export function meetingInviteAccessCode(
  joinedRoom: Pick<RoomRecord, 'accessCode'> | null | undefined,
  savedRoom: Pick<RoomRecord, 'accessCode'> | null | undefined,
  routeRoomName: string | null | undefined
): string | null {
  return (
    publicInviteAccessCode(joinedRoom?.accessCode) ??
    publicInviteAccessCode(savedRoom?.accessCode) ??
    publicInviteAccessCode(routeRoomName)
  );
}

export function inviteCopyAriaLabel(value: string | null | undefined): string {
  const accessCode = publicInviteAccessCode(value);
  return accessCode ? `Room ID ${accessCode}, click to copy invite` : 'Copy invite link';
}

export function inviteCopyTooltip(value: string | null | undefined): string {
  const accessCode = publicInviteAccessCode(value);
  return accessCode ? `Room ID: ${accessCode} (click to copy invite)` : 'Copy invite link';
}

export function inviteLinkCopiedToastMessage(link: string): string {
  return `${INVITE_LINK_COPIED_LABEL}\n${link}`;
}

export function inviteLinkForAccessCode(label: string, accessCode: string | null | undefined): string | null {
  const invitePath = buildMeetingInvitePath(label, accessCode);
  return invitePath ? `${INVITE_ORIGIN}${invitePath}` : null;
}

export function inviteLinkForRoom(
  room: Pick<RoomRecord, 'name' | 'accessCode' | 'displayName'>,
  fallbackLabel?: string
): string | null {
  const label =
    fallbackLabel?.trim() ||
    room.displayName?.trim() ||
    meetingDisplayLabelFromCredential(room.name) ||
    room.name;
  return inviteLinkForAccessCode(label, room.accessCode || accessCodeForCredential(room.name));
}
