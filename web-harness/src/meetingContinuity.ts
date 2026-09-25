import { DisconnectReason } from 'livekit-client';
import { accessCodeForCredential } from '@petal/shared/logic/meetingCode';
import { HARNESS_REJOIN_SESSION_KEY } from './constants';
import type { LogKind } from './ui/logging';

// ---------------------------------------------------------------------------
// #244: keep a browser meeting from ending by accident, and explain the
// endings the user did not choose. On an Android phone the maintainer was
// thrown out of a meeting onto the invite page and never learned why; each
// path that can do that is handled here:
// - Back: one guard history entry per meeting. Popping it asks "Leave
//   meeting?" instead of leaving.
// - Reload (Android pull-to-refresh, a discarded tab being restored): the
//   address bar holds the invite link, which api/j.ts serves as the invite
//   page. This tab's sessionStorage names the meeting, and the invite page
//   sends a reloaded tab that is in it straight back into the web app.
// - Background: LiveKit disconnects itself when the page is frozen or put in
//   the back-forward cache, and the network can die while the phone sleeps.
//   Such a drop rejoins once, by itself, when the page is visible again.
// - Every other disconnect the user did not ask for shows a notice that says
//   why when that is known, with Rejoin, instead of the silent jump to the
//   home screen.
// ---------------------------------------------------------------------------

/** The guard entry's history.state key. Lockstep with MEETING_GUARD_STATE_KEY
 * in api/j.ts: the state survives a reload and a session restore, so the
 * invite page can tell a meeting's guard entry from the entry before it. */
export const MEETING_GUARD_STATE_KEY = 'petalMeetingGuard';

// Pressing these does not count as a user activation, so pushing the guard
// from them would still leave it skippable.
const NON_ACTIVATING_KEYS: ReadonlySet<string> = new Set([
  'Escape',
  'Shift',
  'Control',
  'Alt',
  'AltGraph',
  'Meta',
  'OS',
  'CapsLock',
  'NumLock',
  'ScrollLock',
  'Fn',
  'FnLock',
  'Hyper',
  'Super',
  'Symbol',
  'SymbolLock',
]);

export type DropCause = 'duplicate' | 'removed' | 'closed' | 'network' | 'background' | 'unknown';

/** Why a meeting the user did not leave ended. `pageLeft`: LiveKit's own
 * client-initiated disconnect because the page was frozen, cached or unloaded. */
export function dropCause(reason: unknown, pageLeft: boolean): DropCause {
  if (pageLeft) return 'background';
  switch (reason) {
    case DisconnectReason.DUPLICATE_IDENTITY:
      return 'duplicate';
    case DisconnectReason.PARTICIPANT_REMOVED:
      return 'removed';
    case DisconnectReason.ROOM_DELETED:
    case DisconnectReason.ROOM_CLOSED:
      return 'closed';
    // No reason at all is LiveKit giving up its reconnect attempts.
    case undefined:
    case DisconnectReason.SIGNAL_CLOSE:
    case DisconnectReason.CONNECTION_TIMEOUT:
    case DisconnectReason.JOIN_FAILURE:
    case DisconnectReason.STATE_MISMATCH:
    case DisconnectReason.SERVER_SHUTDOWN:
    case DisconnectReason.MIGRATION:
    case DisconnectReason.MEDIA_FAILURE:
      return 'network';
    default:
      return 'unknown';
  }
}

export interface DropNotice {
  title: string;
  detail: string | null;
  rejoinLabel: string;
  /** The emphasized, focused action. Home when rejoining would undo
   * something someone chose (a kick, closing the room). */
  primary: 'rejoin' | 'home';
}

export const DROP_NOTICES: Record<DropCause, DropNotice> = {
  duplicate: {
    title: 'Joined from another tab or device',
    detail: 'This meeting is open somewhere else under your name. Rejoin here to move the call to this tab.',
    rejoinLabel: 'Rejoin here',
    primary: 'rejoin',
  },
  removed: { title: 'Removed from the meeting', detail: null, rejoinLabel: 'Rejoin', primary: 'home' },
  closed: { title: 'The meeting was closed', detail: null, rejoinLabel: 'Rejoin', primary: 'home' },
  network: { title: 'Connection lost', detail: 'Check your connection, then rejoin.', rejoinLabel: 'Rejoin', primary: 'rejoin' },
  background: {
    title: 'Paused in the background',
    detail: 'Your browser paused the meeting while the page was in the background.',
    rejoinLabel: 'Rejoin',
    primary: 'rejoin',
  },
  unknown: { title: 'You were disconnected', detail: null, rejoinLabel: 'Rejoin', primary: 'rejoin' },
};

// Rejoining after a kick, a duplicate identity (another tab would be kicked
// in turn) or a closed room would undo something deliberate.
const AUTO_REJOIN_CAUSES: ReadonlySet<DropCause> = new Set(['network', 'background']);

export interface MeetingDisconnect {
  /** The user left: Leave, "Leave" in the Back confirm (noteLeaveRequested). */
  requested: boolean;
  /** LiveKit reports CLIENT_INITIATED: `room.disconnect()` from our code, or
   * LiveKit's own page-leave handling. */
  clientInitiated: boolean;
  reason: unknown;
}

export interface MeetingContinuity {
  /** Connected to `meetingCode` (the internal credential). */
  entered: (meetingCode: string) => void;
  /** The room disconnected: back to the home screen when the user left,
   * otherwise the notice (and maybe one automatic rejoin). `settled` runs
   * once the address bar no longer holds the invite link. False, and nothing
   * done, for a room that never entered the meeting: the caller handles it. */
  disconnected: (meetingCode: string | null, disconnect: MeetingDisconnect, settled: () => void) => boolean;
  /** A join failed. True when it was a rejoin from the notice, which then
   * shows `message` instead of the caller falling back to the home screen. */
  rejoinFailed: (message: string) => boolean;
}

type ContinuityWindow = Pick<Window, 'addEventListener' | 'document' | 'history' | 'location' | 'navigator' | 'sessionStorage'>;

export interface MeetingContinuityOptions {
  leaveDialog: HTMLDialogElement;
  notice: {
    title: HTMLElement;
    detail: HTMLElement;
    rejoin: HTMLButtonElement;
    home: HTMLButtonElement;
    /** Always rendered (the notice itself is display:none until shown), so
     * screen readers announce what changed. */
    announcer: HTMLElement;
  };
  showJoinScreen: () => void;
  showDisconnectedScreen: () => void;
  /** The Leave button's action. */
  leave: () => Promise<void>;
  /** Join `meetingCode` again; resolves true once connected. */
  rejoin: (meetingCode: string) => Promise<boolean>;
  logEvent: (message: string, kind?: LogKind) => void;
  win?: ContinuityWindow;
}

export function setupMeetingContinuity(options: MeetingContinuityOptions): MeetingContinuity {
  const { leaveDialog, notice, logEvent } = options;
  const win = options.win ?? window;
  const doc = win.document;
  const history = win.history;
  // Tags this page load's guard entries: after a reload the old document's
  // guard is still in the history list, and must never be unwound as ours.
  const pageId = Math.random().toString(36).slice(2);

  let meeting: string | null = null; // credential while connected
  let guarded = false; // our guard entry is the current history entry
  let awaitingGesture = false;
  let unwound: (() => void) | null = null; // runs once history.back() lands
  let dropped: { meetingCode: string; cause: DropCause } | null = null;
  // The drop's scrub-registry reset: the notice keeps the invite link in the
  // address bar, so the reset waits until the notice goes home.
  let droppedSettled: (() => void) | null = null;
  let autoRejoin: string | null = null; // rejoin when the page is visible again
  let autoRejoinsLeft = 0; // one per return of the page
  let rejoining = false;
  let rejoinError: string | null = null;
  let backDuringRejoin = false;
  // A Back cancels the activation Chrome honours for history entries, but
  // not navigator.userActivation.isActive: only a tap or key press after it
  // makes a new guard entry count.
  let gestureSinceBack = true;
  let pageLeave: 'suspended' | 'unloading' | null = null;

  function isOurGuard(state: unknown): boolean {
    return typeof state === 'object' && state !== null && (state as Record<string, unknown>)[MEETING_GUARD_STATE_KEY] === pageId;
  }

  // This tab only: a reload of the invite link rejoins (api/j.ts). Never the
  // internal credential, never logged, cleared on a deliberate leave.
  function rememberMeeting(accessCode: string | null) {
    try {
      if (accessCode) win.sessionStorage.setItem(HARNESS_REJOIN_SESSION_KEY, accessCode);
      else win.sessionStorage.removeItem(HARNESS_REJOIN_SESSION_KEY);
    } catch {
      // Storage blocked: a reload shows the invite page, as before #244.
    }
  }

  function pushGuard() {
    awaitingGesture = false;
    history.pushState({ [MEETING_GUARD_STATE_KEY]: pageId }, '', win.location.href);
    guarded = true;
  }

  // Chrome's Back skips every entry of a document that pushed another without
  // a user activation behind it, and a Back itself cancels the activation the
  // page had. So push only while a tap or key press is active -- right away
  // when the join came from one, otherwise on the next one.
  function pushGuardWhenActive() {
    if (!gestureSinceBack || win.navigator.userActivation?.isActive === false) awaitingGesture = true;
    else pushGuard();
  }

  function onGesture(event: Event) {
    const key = (event as Partial<KeyboardEvent>).key;
    if (key !== undefined && NON_ACTIVATING_KEYS.has(key)) return;
    if (win.navigator.userActivation?.isActive === false) return;
    gestureSinceBack = true;
    if (awaitingGesture && meeting !== null) pushGuard();
  }

  function hideNotice() {
    dropped = null;
    droppedSettled = null;
    notice.announcer.textContent = '';
  }

  function returnHome(settled: () => void = droppedSettled ?? (() => {})) {
    hideNotice();
    autoRejoin = null;
    rememberMeeting(null);
    const unwind = guarded && isOurGuard(history.state);
    guarded = false;
    history.replaceState(null, '', win.location.origin);
    options.showJoinScreen();
    // Drop the guard entry too, so Back from the home screen goes where it
    // went before the meeting (the invite page, or off the site).
    if (unwind) {
      unwound = settled;
      history.back();
    } else {
      settled();
    }
  }

  function showNotice(meetingCode: string, cause: DropCause, detail: string | null) {
    const copy = DROP_NOTICES[cause];
    dropped = { meetingCode, cause };
    notice.title.textContent = copy.title;
    notice.detail.textContent = detail ?? '';
    notice.detail.hidden = !detail;
    notice.rejoin.textContent = copy.rejoinLabel;
    const primary = copy.primary === 'home' ? notice.home : notice.rejoin;
    const secondary = primary === notice.home ? notice.rejoin : notice.home;
    primary.classList.replace('ghost', 'primary');
    secondary.classList.replace('primary', 'ghost');
    options.showDisconnectedScreen();
    notice.announcer.textContent = detail ? `${copy.title}. ${detail}` : copy.title;
    primary.focus();
  }

  async function rejoin(meetingCode: string) {
    if (rejoining) return;
    rejoining = true;
    rejoinError = null;
    backDuringRejoin = false;
    autoRejoin = null;
    try {
      const joined = await options.rejoin(meetingCode);
      if (backDuringRejoin) {
        // Back pressed on "Joining…": what Back would have done where it
        // lands -- ask in the meeting, go home from the notice.
        if (joined && meeting !== null) openLeaveDialog();
        else if (!joined && dropped !== null) returnHome();
      } else if (!joined && dropped?.meetingCode === meetingCode) {
        showNotice(meetingCode, dropped.cause, rejoinError ?? 'Could not rejoin. Check your connection and try again.');
      }
    } finally {
      rejoining = false;
      backDuringRejoin = false;
    }
  }

  function openLeaveDialog() {
    leaveDialog.returnValue = '';
    if (!leaveDialog.open) leaveDialog.showModal();
  }

  function autoRejoinNow(meetingCode: string, why: string) {
    autoRejoinsLeft = 0;
    logEvent(`${why}; rejoining once`, 'warn');
    // Next task: a caller in the middle of a Disconnected handler is still
    // tearing the old session down.
    setTimeout(() => void rejoin(meetingCode), 0);
  }

  function notePageLeave(kind: 'suspended' | 'unloading') {
    if (meeting !== null && pageLeave !== 'unloading') pageLeave = kind;
  }

  function pageReturned() {
    if (doc.visibilityState !== 'visible') return;
    // A download or mailto: link fires beforeunload, and the page stays.
    if (pageLeave === 'unloading') pageLeave = null;
    autoRejoinsLeft = 1;
    const meetingCode = autoRejoin;
    autoRejoin = null;
    if (meetingCode !== null) autoRejoinNow(meetingCode, 'page visible again after a disconnect');
  }

  function entered(meetingCode: string) {
    meeting = meetingCode;
    hideNotice();
    autoRejoin = null;
    pageLeave = null;
    rememberMeeting(accessCodeForCredential(meetingCode));
    if (isOurGuard(history.state)) guarded = true;
    else pushGuardWhenActive();
  }

  function disconnected(
    meetingCode: string | null,
    { requested, clientInitiated, reason }: MeetingDisconnect,
    settled: () => void
  ): boolean {
    // Connected, but dropped before the join finished: there is no meeting
    // to explain or rejoin.
    if (meeting === null) return false;
    const pageLeft = pageLeave;
    pageLeave = null;
    meeting = null;
    awaitingGesture = false;
    if (leaveDialog.open) leaveDialog.close();
    // A client-initiated disconnect is the user's (or our code's) own doing,
    // unless LiveKit did it because the page was frozen, cached or unloaded.
    if (requested || meetingCode === null || (clientInitiated && pageLeft === null)) {
      returnHome(settled);
      return true;
    }
    const cause = dropCause(reason, clientInitiated);
    logEvent(`disconnected without leaving (${cause})`, 'warn');
    showNotice(meetingCode, cause, DROP_NOTICES[cause].detail);
    droppedSettled = settled;
    // An unloading page is being reloaded or closed: the next page rejoins.
    if (!AUTO_REJOIN_CAUSES.has(cause) || pageLeft === 'unloading') return true;
    if (doc.visibilityState === 'hidden') autoRejoin = meetingCode;
    // The first drop after the page came back is the background's doing, even
    // when LiveKit takes its time to give up reconnecting.
    else if (autoRejoinsLeft > 0) autoRejoinNow(meetingCode, 'disconnected after the page came back');
    return true;
  }

  function rejoinFailed(message: string): boolean {
    if (!rejoining) return false;
    rejoinError = message;
    return true;
  }

  win.addEventListener('popstate', (event) => {
    if (unwound) {
      const settled = unwound;
      unwound = null;
      history.replaceState(null, '', win.location.origin);
      settled();
      return;
    }
    if (!guarded || isOurGuard(event.state)) return;
    // Back popped the guard: we are on the entry below it.
    guarded = false;
    gestureSinceBack = false;
    if (meeting !== null) openLeaveDialog();
    else if (rejoining) backDuringRejoin = true;
    else if (dropped !== null) returnHome();
  });

  // The dialog's form uses method="dialog", so returnValue is the button's
  // value. Only a Stay click (or key press) carries the activation a new
  // guard needs; Escape and the Android back gesture re-arm on the next tap.
  leaveDialog.addEventListener('close', () => {
    if (meeting === null) return;
    if (leaveDialog.returnValue === 'leave') {
      void options.leave();
    } else if (!guarded) {
      if (leaveDialog.returnValue === 'stay') pushGuardWhenActive();
      else awaitingGesture = true;
    }
  });

  notice.rejoin.addEventListener('click', () => {
    if (dropped !== null) void rejoin(dropped.meetingCode);
  });
  notice.home.addEventListener('click', () => {
    if (dropped !== null && !rejoining) returnHome();
  });

  win.addEventListener('pointerup', onGesture, true);
  win.addEventListener('keydown', onGesture, true);
  // Registered before any room connects, so these run before LiveKit's own
  // page-leave listeners start their disconnect.
  win.addEventListener('beforeunload', () => notePageLeave('unloading'));
  win.addEventListener('pagehide', (event) => notePageLeave(event.persisted ? 'suspended' : 'unloading'));
  win.addEventListener('pageshow', (event) => {
    if (event.persisted) pageReturned();
  });
  doc.addEventListener('freeze', () => notePageLeave('suspended'));
  doc.addEventListener('resume', pageReturned);
  doc.addEventListener('visibilitychange', pageReturned);

  return { entered, disconnected, rejoinFailed };
}
