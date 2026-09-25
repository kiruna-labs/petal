import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { DisconnectReason } from 'livekit-client';

import {
  DROP_NOTICES,
  MEETING_GUARD_STATE_KEY,
  dropCause,
  setupMeetingContinuity,
  type MeetingContinuity,
  type MeetingDisconnect,
} from '../src/meetingContinuity.ts';
import { HARNESS_REJOIN_SESSION_KEY } from '../src/constants.ts';
import { MEETING_GUARD_STATE_KEY as INVITE_PAGE_GUARD_STATE_KEY, REJOIN_SESSION_KEY } from '../api/j.ts';
import { internalCredentialForAccessCode } from '@petal/shared/logic/meetingCode';
import { FakeBrowserPage, FakeDialog, FakeElement, settle } from './fixtures/fakeBrowserPage.ts';

// ---------------------------------------------------------------------------
// #244: Back asks before leaving, a reload rejoins (this tab's
// sessionStorage), a background drop rejoins once, and any other disconnect
// the user did not ask for is explained, with Rejoin, instead of silently
// landing on the home screen.
// ---------------------------------------------------------------------------

const ACCESS_CODE = 'abc-defg-hjk';
const CREDENTIAL = internalCredentialForAccessCode(ACCESS_CODE);
const ORIGIN = 'https://meet.petal.live';
const INVITE_URL = `${ORIGIN}/design-review/${ACCESS_CODE}`;

const LEFT: MeetingDisconnect = { requested: true, clientInitiated: true, reason: DisconnectReason.CLIENT_INITIATED };
const KICKED: MeetingDisconnect = { requested: false, clientInitiated: false, reason: DisconnectReason.PARTICIPANT_REMOVED };
const NETWORK: MeetingDisconnect = { requested: false, clientInitiated: false, reason: undefined };
const PAGE_LEFT: MeetingDisconnect = { requested: false, clientInitiated: true, reason: DisconnectReason.CLIENT_INITIATED };

function setup({ rejoinError = null as string | null, rejoinGate = null as Promise<void> | null } = {}) {
  const page = new FakeBrowserPage(INVITE_URL);
  const dialog = new FakeDialog();
  const notice = {
    title: new FakeElement(),
    detail: new FakeElement(),
    rejoin: new FakeElement('btn primary'),
    home: new FakeElement('btn ghost'),
    announcer: new FakeElement('sr-only'),
  };
  const calls = { home: 0, notices: 0, leaves: 0, rejoins: [] as string[] };
  // eslint-disable-next-line prefer-const
  let continuity: MeetingContinuity;
  continuity = setupMeetingContinuity({
    leaveDialog: dialog as unknown as HTMLDialogElement,
    notice: notice as unknown as Parameters<typeof setupMeetingContinuity>[0]['notice'],
    showJoinScreen: () => {
      calls.home += 1;
    },
    showDisconnectedScreen: () => {
      calls.notices += 1;
    },
    leave: async () => {
      calls.leaves += 1;
    },
    // What connection.ts does: a failed join reports its message first.
    rejoin: async (code) => {
      calls.rejoins.push(code);
      if (rejoinGate) await rejoinGate;
      if (rejoinError === null) {
        continuity.entered(code);
        return true;
      }
      assert.equal(continuity.rejoinFailed(rejoinError), true, 'a rejoin is in flight');
      return false;
    },
    logEvent: () => {},
    win: page.window as unknown as Window,
  });
  const stored = () => page.sessionStorage.getItem(HARNESS_REJOIN_SESSION_KEY);
  return { page, dialog, notice, calls, continuity, stored };
}

const noop = () => {};

test('the web client and the invite page agree on the sessionStorage key and the guard state key', () => {
  assert.equal(HARNESS_REJOIN_SESSION_KEY, REJOIN_SESSION_KEY);
  assert.equal(MEETING_GUARD_STATE_KEY, INVITE_PAGE_GUARD_STATE_KEY);
});

test('the guard entry carries the state key the invite page looks for', () => {
  const { page, continuity } = setup();

  continuity.entered(CREDENTIAL);

  assert.ok((page.history.state as Record<string, unknown>)[MEETING_GUARD_STATE_KEY]);
});

test('a disconnect for a join that never entered the meeting is left to the join (LiveKit reports it before connect() rejects)', async () => {
  const { page, notice, calls, continuity } = setup();
  page.hide();
  page.show(); // an earlier tab switch: a drop now would rejoin by itself
  let settled = 0;

  continuity.disconnected(CREDENTIAL, { requested: false, clientInitiated: false, reason: DisconnectReason.JOIN_FAILURE }, () => (settled += 1));
  await settle();

  assert.equal(calls.notices, 0, 'no notice over "Joining…"');
  assert.equal(notice.announcer.textContent, '', 'nothing announced');
  assert.deepEqual(calls.rejoins, [], 'no second join racing the retry');
  assert.equal(calls.home, 0);
  assert.equal(settled, 0, 'the join still owns its session');
});

test('a Rejoin whose connect fails keeps the cause of the original drop', async () => {
  const { notice, continuity } = setup({ rejoinError: 'Connect failed: could not establish signal connection' });
  continuity.entered(CREDENTIAL);
  continuity.disconnected(CREDENTIAL, KICKED, noop);

  // The rejoin's own failed attempt: Disconnected first, then the rejection.
  notice.rejoin.click();
  continuity.disconnected(CREDENTIAL, { requested: false, clientInitiated: false, reason: DisconnectReason.UNKNOWN_REASON }, noop);
  await settle();

  assert.equal(notice.title.textContent, 'Removed from the meeting');
  assert.equal(notice.detail.textContent, 'Connect failed: could not establish signal connection');
});

test('Back while a rejoin is in flight asks once it lands, and never re-arms on the tap from before the Back', async () => {
  let release!: () => void;
  const { page, dialog, notice, calls, continuity } = setup({ rejoinGate: new Promise<void>((resolve) => (release = resolve)) });
  continuity.entered(CREDENTIAL);
  continuity.disconnected(CREDENTIAL, KICKED, noop);

  page.navigator.userActivation = { isActive: true };
  page.window.dispatch('pointerup', {});
  notice.rejoin.click();
  page.history.userBack(); // on "Joining…"
  await settle();
  assert.equal(calls.home, 0);
  release();
  await settle();

  assert.deepEqual(calls.rejoins, [CREDENTIAL]);
  assert.equal(dialog.open, true, 'the Back is answered where it landed: in the meeting');
  assert.equal(page.history.index, 0, 'isActive is still true from the old tap, but a Back cancelled it for history');
  dialog.answer('stay');
  await settle();
  assert.equal(page.history.index, 0, 'still not: no tap since the Back');
  page.window.dispatch('pointerup', {});
  assert.equal(page.history.index, 1, 'the next tap re-arms the guard');
});

test('Back while a rejoin is in flight that then fails goes home', async () => {
  let release!: () => void;
  const { page, notice, calls, continuity, stored } = setup({
    rejoinError: 'Token request failed: Failed to fetch',
    rejoinGate: new Promise<void>((resolve) => (release = resolve)),
  });
  continuity.entered(CREDENTIAL);
  continuity.disconnected(CREDENTIAL, KICKED, noop);

  notice.rejoin.click();
  page.history.userBack();
  await settle();
  release();
  await settle();

  assert.equal(calls.home, 1);
  assert.equal(stored(), null);
});

test('a beforeunload that did not unload (a download link) stops counting once the page is seen again', async () => {
  const { page, calls, continuity } = setup();
  continuity.entered(CREDENTIAL);
  page.window.dispatch('beforeunload');
  page.hide();
  page.show();

  // Our own code disconnecting (cockpit, automation) is a leave again.
  continuity.disconnected(CREDENTIAL, PAGE_LEFT, noop);
  await settle();

  assert.equal(calls.home, 1);
  assert.equal(calls.notices, 0);
});

test('joining remembers the public access code for this tab only and pushes one Back guard', () => {
  const { page, continuity, stored } = setup();

  continuity.entered(CREDENTIAL);

  assert.equal(stored(), ACCESS_CODE, 'never the internal credential');
  assert.deepEqual(page.history.urls(), [INVITE_URL, INVITE_URL]);
  assert.equal(page.history.index, 1);
  assert.deepEqual(page.history.calls.map(([kind]) => kind), ['push']);
});

test('blocked storage never breaks the join (a reload then shows the invite page, as before)', () => {
  const { page, continuity } = setup();
  page.sessionStorage.blocked = true;

  continuity.entered(CREDENTIAL);

  assert.equal(page.history.length, 2);
});

test('a rejoin while the guard is still the current entry does not stack a second guard', () => {
  const { page, continuity } = setup();
  continuity.entered(CREDENTIAL);
  continuity.disconnected(CREDENTIAL, KICKED, noop);

  continuity.entered(CREDENTIAL);

  assert.equal(page.history.length, 2);
});

test('Back during a meeting asks "Leave meeting?"; a Stay click keeps the meeting and re-arms the guard', async () => {
  const { page, dialog, calls, continuity } = setup();
  continuity.entered(CREDENTIAL);

  page.history.userBack();
  await settle();

  assert.equal(dialog.open, true);
  assert.equal(page.history.index, 0);

  // The Stay tap: its pointerup (an activation), then the form's close.
  page.navigator.userActivation = { isActive: true };
  page.window.dispatch('pointerup', {});
  assert.equal(page.history.index, 0, 'the tap alone does not push while the dialog asks');
  dialog.answer('stay');
  await settle();

  assert.equal(calls.leaves, 0, 'Stay never disconnects');
  assert.equal(calls.home, 0);
  assert.equal(page.history.index, 1, 'the guard is back for the next Back');
  assert.equal(page.history.length, 2);

  page.history.userBack();
  await settle();
  assert.equal(dialog.open, true, 'the next Back asks again');
});

test('Escape/back-gesture Stay re-arms only on the next gesture', async () => {
  const { page, dialog, calls, continuity } = setup();
  continuity.entered(CREDENTIAL);
  page.history.userBack();
  await settle();

  // Chrome drops the page's activation on a Back, and neither Escape nor the
  // back gesture gives one: a guard pushed now would be skippable.
  page.navigator.userActivation = { isActive: false };
  dialog.cancel();
  await settle();
  assert.equal(calls.leaves, 0);
  assert.equal(page.history.length, 2);
  assert.equal(page.history.index, 0, 'no guard pushed without an activation');

  page.window.dispatch('keydown', { key: 'Escape' });
  page.window.dispatch('keydown', { key: 'Shift' });
  assert.equal(page.history.index, 0, 'Escape and modifier keys are not activations');

  page.navigator.userActivation = { isActive: true };
  page.window.dispatch('keydown', { key: 'Escape' });
  page.window.dispatch('keydown', { key: 'Shift' });
  assert.equal(page.history.index, 0, 'Escape and modifiers are ignored even while another activation is live');
  page.window.dispatch('pointerup', {});
  assert.equal(page.history.index, 1, 'the next tap re-arms the guard');
  assert.equal(page.history.length, 2);
});

test('Back then Leave disconnects as a deliberate leave: home, rejoin code cleared, address bar at the origin', async () => {
  const { page, dialog, calls, continuity, stored } = setup();
  continuity.entered(CREDENTIAL);
  page.history.userBack();
  await settle();

  dialog.answer('leave');
  await settle();
  assert.equal(calls.leaves, 1);
  let settled = 0;
  // The Leave action's room.disconnect() then reports back:
  continuity.disconnected(CREDENTIAL, LEFT, () => (settled += 1));
  await settle();

  assert.equal(calls.home, 1);
  assert.equal(calls.notices, 0);
  assert.equal(stored(), null);
  assert.equal(page.location.href, `${ORIGIN}/`);
  assert.equal(settled, 1);
  assert.ok(!page.history.calls.some(([kind]) => kind === 'back'), 'Back already popped the guard');
});

test('the Leave button unwinds the guard, and reports settled only once the invite link is gone', async () => {
  const { page, calls, continuity, stored } = setup();
  continuity.entered(CREDENTIAL);
  const urlsWhenSettled: string[][] = [];

  continuity.disconnected(CREDENTIAL, LEFT, () => urlsWhenSettled.push(page.history.urls()));
  assert.deepEqual(urlsWhenSettled, [], 'not before history.back() lands');
  await settle();

  assert.equal(calls.home, 1);
  assert.equal(stored(), null);
  assert.equal(page.history.index, 0, 'the guard entry was stepped off');
  assert.equal(page.location.href, `${ORIGIN}/`);
  assert.deepEqual(urlsWhenSettled, [[`${ORIGIN}/`, `${ORIGIN}/`]]);
});

test('a client-initiated disconnect from our own code (cockpit, automation) is still a leave', () => {
  const { calls, continuity } = setup();
  continuity.entered(CREDENTIAL);

  continuity.disconnected(CREDENTIAL, PAGE_LEFT, noop);

  assert.equal(calls.home, 1);
  assert.equal(calls.notices, 0);
});

test('a server kick says so, leads with Back to home, and is never undone automatically', async () => {
  const { page, notice, calls, continuity, stored } = setup();
  continuity.entered(CREDENTIAL);

  continuity.disconnected(CREDENTIAL, KICKED, noop);
  page.hide();
  page.show();
  await settle();

  assert.equal(calls.home, 0, 'no silent jump to the home screen');
  assert.equal(calls.notices, 1);
  assert.equal(notice.title.textContent, 'Removed from the meeting');
  assert.equal(notice.detail.hidden, true);
  assert.ok(notice.home.classes.has('primary') && notice.home.focused, 'Back to home is the primary action');
  assert.ok(notice.rejoin.classes.has('ghost') && !notice.rejoin.classes.has('primary'));
  assert.equal(notice.announcer.textContent, 'Removed from the meeting');
  assert.deepEqual(calls.rejoins, []);
  assert.equal(stored(), ACCESS_CODE, 'a reload still rejoins');
  assert.equal(page.location.href, INVITE_URL, 'the invite link stays in the address bar');

  notice.rejoin.click();
  await settle();
  assert.deepEqual(calls.rejoins, [CREDENTIAL]);
});

test('a duplicate identity offers "Rejoin here", which moves the call to this tab, and never rejoins by itself', async () => {
  const { page, notice, calls, continuity } = setup();
  continuity.entered(CREDENTIAL);
  page.hide();

  continuity.disconnected(CREDENTIAL, { requested: false, clientInitiated: false, reason: DisconnectReason.DUPLICATE_IDENTITY }, noop);
  page.show();
  await settle();

  assert.equal(notice.title.textContent, 'Joined from another tab or device');
  assert.match(notice.detail.textContent, /move the call to this tab/);
  assert.equal(notice.rejoin.textContent, 'Rejoin here');
  assert.ok(notice.rejoin.classes.has('primary') && notice.rejoin.focused);
  assert.deepEqual(calls.rejoins, [], 'rejoining by itself would kick the other tab in turn');
});

test('a network drop while the page is hidden rejoins once when it is visible again', async () => {
  const { page, notice, calls, continuity } = setup();
  continuity.entered(CREDENTIAL);
  page.hide();

  continuity.disconnected(CREDENTIAL, NETWORK, noop);
  await settle();
  assert.equal(notice.title.textContent, 'Connection lost');
  assert.deepEqual(calls.rejoins, [], 'nothing happens while hidden');

  page.show();
  await settle();
  assert.deepEqual(calls.rejoins, [CREDENTIAL]);
  assert.equal(notice.announcer.textContent, '', 'the notice is gone once rejoined');
});

test('two drops after one return: one automatic rejoin, then the notice', async () => {
  const { page, notice, calls, continuity } = setup();
  continuity.entered(CREDENTIAL);
  page.hide();
  page.show();

  continuity.disconnected(CREDENTIAL, NETWORK, noop);
  await settle();
  assert.deepEqual(calls.rejoins, [CREDENTIAL]);

  continuity.disconnected(CREDENTIAL, NETWORK, noop);
  await settle();

  assert.deepEqual(calls.rejoins, [CREDENTIAL], 'no second automatic rejoin');
  assert.equal(notice.title.textContent, 'Connection lost');
  assert.equal(calls.notices, 2);
});

test('the first drop after a return rejoins however long LiveKit took to give up', async () => {
  const { page, calls, continuity } = setup();
  continuity.entered(CREDENTIAL);
  page.hide();
  page.show();
  // (No clock: there is no time window to fall out of.)

  continuity.disconnected(CREDENTIAL, NETWORK, noop);
  await settle();

  assert.deepEqual(calls.rejoins, [CREDENTIAL]);
});

test('a network drop while the user is looking, with no return, shows the notice and waits for Rejoin', async () => {
  const { notice, calls, continuity } = setup();
  continuity.entered(CREDENTIAL);

  continuity.disconnected(CREDENTIAL, NETWORK, noop);
  await settle();

  assert.equal(notice.title.textContent, 'Connection lost');
  assert.equal(notice.detail.textContent, 'Check your connection, then rejoin.');
  assert.deepEqual(calls.rejoins, []);
});

test('pageshow from the back-forward cache also triggers the one automatic rejoin', async () => {
  const { page, notice, calls, continuity } = setup();
  continuity.entered(CREDENTIAL);
  page.window.dispatch('pagehide', { persisted: true });
  page.document.visibilityState = 'hidden';

  continuity.disconnected(CREDENTIAL, PAGE_LEFT, noop);
  page.document.visibilityState = 'visible';
  page.window.dispatch('pageshow', { persisted: true });
  await settle();

  assert.equal(notice.title.textContent, DROP_NOTICES.background.title);
  assert.deepEqual(calls.rejoins, [CREDENTIAL]);
});

test('LiveKit disconnecting a frozen page is a drop, not a leave, and rejoins on resume', async () => {
  const { page, calls, continuity, stored } = setup();
  continuity.entered(CREDENTIAL);
  page.document.visibilityState = 'hidden';
  page.document.dispatch('freeze');

  // LiveKit's own freeze listener disconnects (CLIENT_INITIATED); the event
  // often only lands once the page runs again, i.e. after resume.
  page.document.visibilityState = 'visible';
  page.document.dispatch('resume');
  continuity.disconnected(CREDENTIAL, PAGE_LEFT, noop);
  await settle();

  assert.equal(calls.home, 0);
  assert.equal(calls.notices, 1);
  assert.deepEqual(calls.rejoins, [CREDENTIAL]);
  assert.equal(stored(), ACCESS_CODE, 'not a deliberate leave');
});

test('a reload (beforeunload) keeps the rejoin code and never rejoins from the dying page', async () => {
  const { page, calls, continuity, stored } = setup();
  continuity.entered(CREDENTIAL);
  page.hide();
  page.show();
  page.window.dispatch('beforeunload');

  continuity.disconnected(CREDENTIAL, PAGE_LEFT, noop);
  await settle();

  assert.equal(calls.home, 0, 'no history rewrite while the reload is in flight');
  assert.deepEqual(page.history.calls.filter(([kind]) => kind !== 'push'), []);
  assert.equal(stored(), ACCESS_CODE);
  assert.deepEqual(calls.rejoins, [], 'the reloaded page rejoins through the invite page instead');
});

test('a failed rejoin comes back to the notice with the real error, and does not retry by itself', async () => {
  const { page, notice, calls, continuity } = setup({ rejoinError: 'Connect failed: could not establish signal connection' });
  continuity.entered(CREDENTIAL);
  page.hide();
  continuity.disconnected(CREDENTIAL, NETWORK, noop);

  page.show();
  await settle();
  page.hide();
  page.show();
  await settle();

  assert.deepEqual(calls.rejoins, [CREDENTIAL]);
  assert.equal(notice.title.textContent, 'Connection lost');
  assert.equal(notice.detail.textContent, 'Connect failed: could not establish signal connection');
  assert.equal(notice.detail.hidden, false);
  assert.equal(calls.home, 0, 'never the home screen');
  assert.equal(continuity.rejoinFailed('a join from the home screen'), false, 'only a rejoin is taken over');
});

test('Back from the notice goes home and forgets the meeting', async () => {
  const { page, calls, continuity, stored } = setup();
  continuity.entered(CREDENTIAL);
  continuity.disconnected(CREDENTIAL, KICKED, noop);

  page.history.userBack();
  await settle();

  assert.equal(calls.home, 1);
  assert.equal(stored(), null);
  assert.equal(page.location.href, `${ORIGIN}/`);
});

test('"Back to home" on the notice drops the guard and forgets the meeting', async () => {
  const { page, notice, calls, continuity, stored } = setup();
  continuity.entered(CREDENTIAL);
  continuity.disconnected(CREDENTIAL, KICKED, noop);

  notice.home.click();
  await settle();

  assert.equal(calls.home, 1);
  assert.equal(stored(), null);
  assert.equal(page.history.index, 0);
  assert.equal(page.location.href, `${ORIGIN}/`);
});

test('without a user gesture yet, the guard waits for the first tap (Chrome skips gesture-less entries)', () => {
  const { page, continuity } = setup();
  page.navigator.userActivation = { isActive: false };

  continuity.entered(CREDENTIAL);
  assert.equal(page.history.length, 1, 'not pushed without a gesture');

  page.window.dispatch('pointerup', {});
  assert.equal(page.history.length, 1, 'a pointerup that carried no activation does not count');

  page.navigator.userActivation = { isActive: true };
  page.window.dispatch('pointerup', {});
  assert.equal(page.history.length, 2);
  assert.equal(page.history.index, 1);
});

test('the meeting dropping while the confirm is open closes it without leaving', async () => {
  const { page, dialog, notice, calls, continuity } = setup();
  continuity.entered(CREDENTIAL);
  page.history.userBack();
  await settle();

  continuity.disconnected(CREDENTIAL, KICKED, noop);
  await settle();

  assert.equal(dialog.open, false);
  assert.equal(calls.leaves, 0);
  assert.equal(notice.title.textContent, DROP_NOTICES.removed.title);
});

test('disconnect reasons map to what the notice says', () => {
  assert.equal(dropCause(DisconnectReason.DUPLICATE_IDENTITY, false), 'duplicate');
  assert.equal(dropCause(DisconnectReason.PARTICIPANT_REMOVED, false), 'removed');
  assert.equal(dropCause(DisconnectReason.ROOM_DELETED, false), 'closed');
  assert.equal(dropCause(undefined, false), 'network');
  assert.equal(dropCause(DisconnectReason.SIGNAL_CLOSE, false), 'network');
  assert.equal(dropCause(DisconnectReason.CLIENT_INITIATED, true), 'background');
  assert.equal(dropCause(DisconnectReason.UNKNOWN_REASON, false), 'unknown');
  assert.equal(DROP_NOTICES.unknown.title, 'You were disconnected', 'no made-up reason');
  assert.equal(DROP_NOTICES.unknown.detail, null);
  assert.equal(DROP_NOTICES.closed.title, 'The meeting was closed');
  assert.equal(DROP_NOTICES.closed.primary, 'home');
});

// Attribute order and whitespace are free; only what the module relies on is pinned.
function elementWithId(html: string, id: string) {
  for (const match of html.matchAll(/<([a-z0-9]+)\b([^>]*)>/gi)) {
    const attrs = new Map<string, string>();
    for (const attr of match[2]!.matchAll(/([a-z-]+)(?:="([^"]*)")?/gi)) attrs.set(attr[1]!.toLowerCase(), attr[2] ?? '');
    if (attrs.get('id') === id) {
      const tag = match[1]!.toLowerCase();
      const start = match.index!;
      return { tag, attrs, classes: new Set((attrs.get('class') ?? '').split(/\s+/)), start, end: html.indexOf(`</${tag}>`, start) };
    }
  }
  assert.fail(`no element #${id}`);
}

function cssRuleFor(css: string, selectors: string[]): string {
  const normalize = (text: string) => text.replace(/\s+/g, ' ').trim();
  for (const match of css.replace(/\/\*[\s\S]*?\*\//g, '').matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    const list = match[1]!.split(',').map(normalize);
    if (selectors.every((selector) => list.includes(selector))) return normalize(match[2]!);
  }
  assert.fail(`no rule for ${selectors.join(', ')}`);
}

test('the page ships the confirm, the notice with an always-rendered announcer, and no pull-to-refresh in meetings', () => {
  const html = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
  const css = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');

  const dialog = elementWithId(html, 'leave-confirm');
  assert.equal(dialog.tag, 'dialog');
  assert.match(html.slice(dialog.start, dialog.end), /<form\b[^>]*\bmethod="dialog"/, 'returnValue is the button value');
  assert.equal(elementWithId(html, 'leave-confirm-leave').attrs.get('value'), 'leave');
  const stay = elementWithId(html, 'leave-confirm-stay');
  assert.equal(stay.attrs.get('value'), 'stay');
  assert.ok(stay.attrs.has('autofocus') && stay.classes.has('primary'), 'Stay is the default');

  const screen = elementWithId(html, 'disconnected-screen');
  for (const id of ['disconnected-title', 'disconnected-detail', 'disconnected-rejoin', 'disconnected-home']) {
    const element = elementWithId(html, id);
    assert.ok(element.start > screen.start, id);
  }
  const announcer = elementWithId(html, 'disconnected-announcer');
  assert.ok(announcer.attrs.has('aria-live'));
  assert.ok(!announcer.attrs.has('hidden') && !announcer.classes.has('hidden'), 'always rendered');
  assert.ok(announcer.start > html.indexOf('</div>', elementWithId(html, 'disconnected-home').start), 'outside the notice');

  assert.match(
    cssRuleFor(css, ['html:has(#meeting-screen:not(.hidden))', 'html:has(#meeting-screen:not(.hidden)) body']),
    /overscroll-behavior-y: none/
  );
});
