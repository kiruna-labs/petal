// #122 rendered fixture: mounts the REAL MainMenu (frameless, i.e. the menu IS
// the main window) with enough rows to make the room list scroll, and with
// server-side ROSTER names on some of them. The test then drives real pointer
// and keyboard input at it -- CSS `:hover` and `:active` cannot be faked with
// synthetic events, and neither source text nor getBoundingClientRect() can
// see a tooltip clipped by the scrolling list.
import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';
import '@fontsource/albert-sans/700.css';
import '@fontsource/jetbrains-mono/400.css';
import '@fontsource/jetbrains-mono/500.css';
import '@fontsource/jetbrains-mono/700.css';
import { mockIPC } from '@tauri-apps/api/mocks';

// Twelve rows so the list scrolls at the real window height, with the room
// that has a roster placed LAST -- the bottom-of-a-scrolled-list case where a
// below-the-row tooltip would run off the window.
const ROOMS = [
  'design-review',
  'standup',
  'infra-sync',
  'release-train',
  'oncall-handoff',
  'bug-triage',
  'growth-weekly',
  'security-review',
  'platform-guild',
  'hiring-loop',
  'roadmap-review',
  'eng-sync'
];

const LIVE_ROOM = 'eng-sync';
const SECOND_LIVE_ROOM = 'design-review';

// A deliberately unkind roster: the longest realistic names, so the fit is
// measured against a worst case rather than "Ada, Bruno".
const LIVE_ROSTER = [
  'Alexandra Featherstonehaugh',
  'Bartholomew Oyelaran-Whitfield',
  'Chidinma Nwachukwu',
  'Dmitri Konstantinopolous',
  'Evangelina Villanueva-Reyes',
  'Fitzwilliam Ashworth'
];

const ACCESS_CODES = Object.fromEntries(
  ROOMS.map((room, index) => [
    room,
    `${'abcdefghijklm'.slice(index % 5, (index % 5) + 3)}-vera-mol`
  ])
);

function nextFrame() {
  return new Promise((resolve) => requestAnimationFrame(() => resolve()));
}

async function renderFixture() {
  try {
    mockIPC((command) => {
      if (command === 'plugin:event|listen') return 1;
      return null;
    });

    const [{ mount }, { default: MainMenu }] = await Promise.all([
      import('svelte'),
      import('$lib/components/MainMenu.svelte')
    ]);

    mount(MainMenu, {
      target: document.querySelector('#app'),
      props: {
        userName: 'Jordan Kim',
        userIdentity: 'plum',
        frameless: true,
        // No hero: this fixture is the ROW surface, and the hero-promotion
        // regression is exactly what must not happen when names arrive.
        liveRoom: undefined,
        emptyRooms: ROOMS,
        currentRoom: null,
        roomOccupancyByName: { [LIVE_ROOM]: LIVE_ROSTER.length, [SECOND_LIVE_ROOM]: 2 },
        // The #122 field: names, and nothing else, keyed by room.
        roomRosterByName: {
          [LIVE_ROOM]: LIVE_ROSTER,
          [SECOND_LIVE_ROOM]: ['Ada', 'Bruno']
        },
        roomAccessCodesByName: ACCESS_CODES,
        onJoinRoom: () => {},
        onCopyRoomLink: () => true,
        onToggleFavoriteRoom: () => {},
        onRemoveRoom: () => {},
        onCreateMeeting: () => {},
        onJoinByCode: () => {}
      }
    });

    await document.fonts.ready;
    const deadline = performance.now() + 5000;
    while (document.querySelectorAll('.room-row-shell').length < ROOMS.length) {
      if (performance.now() >= deadline) throw new Error('MainMenu did not render every room row');
      await nextFrame();
    }
    await document.fonts.load('600 11px "Albert Sans"', LIVE_ROSTER[0]);
    await document.fonts.ready;
    await nextFrame();
    await nextFrame();

    // Scroll the roster room to the very bottom of the visible list.
    const scroller = document.querySelector('.room-list-scroll');
    if (!scroller) throw new Error('the room list scroller did not render');
    scroller.scrollTop = scroller.scrollHeight;
    await nextFrame();
    await nextFrame();

    document.body.dataset.fixtureReady = 'true';
  } catch (error) {
    const message = error instanceof Error ? `${error.message}\n${error.stack ?? ''}` : String(error);
    document.body.dataset.fixtureError = encodeURIComponent(message);
  }
}

void renderFixture();
