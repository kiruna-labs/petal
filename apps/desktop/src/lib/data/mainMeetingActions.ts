import type { RoomRecord } from '../ipc.ts';
import { meetingActionErrorMessage } from './meetingActionError.ts';

type MeetingNavigationSource = 'create' | 'join';

export type MainMeetingActionsDeps = {
  createRoom: (name: string, open: boolean, displayName?: string | null) => Promise<RoomRecord>;
  goto: (route: string) => Promise<unknown>;
  /** Best-effort pre-navigation resize to the meeting window geometry
   * (Tauri only); never blocks navigation on failure. */
  prepareMeetingWindow?: () => Promise<void>;
  rememberPendingRoomDisplayName?: (name: string, displayName?: string | null) => void;
  setMeetingActionError: (message: string | null) => void;
  logger?: Pick<Console, 'info' | 'error'>;
};

export function createMainMeetingActions({
  createRoom,
  goto,
  prepareMeetingWindow,
  rememberPendingRoomDisplayName,
  setMeetingActionError,
  logger = console
}: MainMeetingActionsDeps) {
  async function navigateToMeeting(room: RoomRecord, source: MeetingNavigationSource) {
    const route = `/meeting/${encodeURIComponent(room.name)}`;
    logger.info(`main-menu: ${source} resolved room; navigating`, {
      route,
      room: room.name,
      hasAccessCode: Boolean(room.accessCode),
      displayName: room.displayName ?? null
    });
    try {
      // Resize before navigating so the swap happens at a constant window
      // size (no desktop flash on the transparent window).
      if (prepareMeetingWindow) {
        try {
          await prepareMeetingWindow();
        } catch {
          // Never block navigation on the pre-size.
        }
      }
      await goto(route);
    } catch (e) {
      logger.error(`Failed to open ${source} meeting route`, e);
      setMeetingActionError(meetingActionErrorMessage(e, 'open'));
    }
  }

  async function joinAndGo(roomName: string, accessCode?: string) {
    // The meeting route owns the real idempotent join. Navigating first keeps
    // /main from briefly rendering its legitimate in-meeting banner mid-join.
    setMeetingActionError(null);
    logger.info('main-menu: join/create requested', {
      input: roomName,
      kind: /^room-[0-9a-f]{32}$/i.test(roomName) ? 'credential' : 'label-or-access-code'
    });
    let room: RoomRecord;
    try {
      room = await createRoom(accessCode ?? roomName, true);
    } catch (e) {
      logger.error('Failed to create/join room', e);
      setMeetingActionError(meetingActionErrorMessage(e, 'join'));
      return;
    }
    await navigateToMeeting(room, 'join');
  }

  async function startMeetingAndGo(roomName: string, displayName: string | null) {
    setMeetingActionError(null);
    logger.info('main-menu: create requested', {
      input: roomName,
      displayName,
      kind: /^room-[0-9a-f]{32}$/i.test(roomName) ? 'credential' : 'label-or-access-code'
    });
    let room: RoomRecord;
    try {
      room = await createRoom(roomName, true, displayName);
    } catch (e) {
      logger.error('Failed to start meeting', e);
      setMeetingActionError(meetingActionErrorMessage(e, 'create'));
      return;
    }
    rememberPendingRoomDisplayName?.(room.name, room.displayName);
    await navigateToMeeting(room, 'create');
  }

  return { joinAndGo, navigateToMeeting, startMeetingAndGo };
}
