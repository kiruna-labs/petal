import { accessCodeForCredential, normalizeAccessCode } from './meetingCode.ts';

export interface RoomAccessCodeRecord {
  name: string;
  accessCode?: string | null;
}

/** Return a canonical user-facing code, never the internal room credential. */
export function roomAccessCode(room: RoomAccessCodeRecord): string | null {
  const persisted = room.accessCode ? normalizeAccessCode(room.accessCode) : null;
  if (persisted) return persisted;
  return accessCodeForCredential(room.name);
}
