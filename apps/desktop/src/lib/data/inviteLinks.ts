import {
  accessCodeForCredential,
  buildMeetingInvitePath,
  meetingDisplayLabelFromCredential,
  normalizeAccessCode
} from './meetingCode.ts';
import type { RoomRecord } from '$lib/ipc';
import {
  DEFAULT_INVITE_ORIGIN,
  normalizeServerOrigin
} from '@petal/shared/logic/serverEndpoints';

// Origin for copied invite links. Settings can override this at runtime
// (#310) without a rebuild. A self-hosted deployment can still bake
// `VITE_PETAL_INVITE_ORIGIN` (docs/SELF_HOSTING.md); the official value is
// the default so official builds need no extra env.
export const INVITE_ORIGIN =
  normalizeServerOrigin(import.meta.env?.VITE_PETAL_INVITE_ORIGIN as string | undefined) ||
  DEFAULT_INVITE_ORIGIN;
export const INVITE_LINK_COPIED_LABEL = 'Invite link copied to clipboard:';

let inviteOriginOverride: string | null = null;

/** Apply a saved Settings override (or `null` to use the baked origin). */
export function applyInviteOrigin(origin: string | null | undefined): string {
  inviteOriginOverride = origin ? normalizeServerOrigin(origin) : null;
  return inviteOrigin();
}

export function inviteOrigin(): string {
  return inviteOriginOverride ?? INVITE_ORIGIN;
}

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
  return invitePath ? `${inviteOrigin()}${invitePath}` : null;
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
