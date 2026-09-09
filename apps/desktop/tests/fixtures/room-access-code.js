// #123 fit fixture: mounts the REAL MainMenu (frameless, i.e. the menu IS the
// 400px main window) with a live hero + room rows that all carry an
// always-visible room ID, then reports rendered-pixel measurements. The point
// is CLAUDE.md's "UI text must NEVER truncate" rule: making the ID permanent
// adds text to every row, so the fit has to be measured with the real fonts,
// not eyeballed.
import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';
import '@fontsource/albert-sans/700.css';
import '@fontsource/jetbrains-mono/400.css';
import '@fontsource/jetbrains-mono/500.css';
import '@fontsource/jetbrains-mono/700.css';
// Manrope is not a @fontsource package: app.css -> styles/fonts.css declares
// the self-hosted variable face the design bundle ships.
import { mockIPC } from '@tauri-apps/api/mocks';

const HERO_ROOM = 'eng-sync';
const HERO_CODE = 'kip-vera-mol';

// A realistic worst case for the row: the long `rctest-<epoch>` names the
// live harness creates, plus a hand-typed display label that is longer still.
const ROOMS = [
  'design-review',
  'rctest-1757442938',
  'rctest-1757442938-cross-machine-regression',
  'standup'
];

const ACCESS_CODES = {
  [HERO_ROOM]: HERO_CODE,
  'design-review': 'tob-suna-rix',
  'rctest-1757442938': 'vel-mara-dun',
  'rctest-1757442938-cross-machine-regression': 'wux-nomo-zek',
  standup: 'qeb-tavu-hos'
};

function nextFrame() {
  return new Promise((resolve) => requestAnimationFrame(() => resolve()));
}

function selectorFor(element) {
  const classes = [...element.classList].map((name) => `.${name}`).join('');
  return `${element.tagName.toLowerCase()}${classes}`;
}

function measureText(element) {
  if (!element) return null;
  const bounds = element.getBoundingClientRect();
  const style = getComputedStyle(element);
  return {
    selector: selectorFor(element),
    text: element.textContent,
    left: bounds.left,
    right: bounds.right,
    width: bounds.width,
    height: bounds.height,
    scrollWidth: element.scrollWidth,
    clientWidth: element.clientWidth,
    opacity: style.opacity,
    visibility: style.visibility,
    display: style.display,
    textOverflow: style.textOverflow,
    fontFamily: style.fontFamily,
    fontSize: style.fontSize
  };
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
        liveRoom: {
          name: HERO_ROOM,
          participants: [
            { name: 'Marco', identity: 'blue' },
            { name: 'Devin', identity: 'lilac' },
            { name: 'Sana', identity: 'green' }
          ]
        },
        emptyRooms: ROOMS,
        // One row is the room this process is joined to (the "live" RoomRow
        // branch: name + status + code stacked), the rest are plain rows.
        currentRoom: 'design-review',
        roomOccupancyByName: { 'rctest-1757442938': 3 },
        roomAccessCodesByName: ACCESS_CODES,
        onJoinLive: () => {},
        onJoinRoom: () => {},
        onCopyRoomLink: () => true,
        onToggleFavoriteRoom: () => {},
        onRemoveRoom: () => {},
        onCreateMeeting: () => {},
        onJoinByCode: () => {}
      }
    });

    await document.fonts.ready;
    const deadline = performance.now() + 3000;
    while (document.querySelectorAll('[data-testid="room-access-code"]').length < ROOMS.length) {
      if (performance.now() >= deadline) throw new Error('MainMenu did not render a room ID on every row');
      await nextFrame();
    }
    if (!document.querySelector('[data-testid="hero-access-code"]')) {
      throw new Error('LiveHero did not render its room ID');
    }

    const monoFaces = await document.fonts.load('600 10px "JetBrains Mono"', HERO_CODE);
    const displayFaces = await document.fonts.load('700 14px "Manrope"', 'design-review');
    await document.fonts.ready;
    await nextFrame();
    await nextFrame();

    const menu = document.querySelector('.main-menu');
    if (!menu) throw new Error('main menu did not mount');

    const codes = [...document.querySelectorAll('[data-testid="room-access-code"]')].map(measureText);
    const heroCode = measureText(document.querySelector('[data-testid="hero-access-code"]'));
    const names = [...menu.querySelectorAll('.room-name')].map(measureText);
    const statuses = [...menu.querySelectorAll('.room-status')].map(measureText);
    const heroTitle = measureText(menu.querySelector('.title'));

    // Every element in the real main window, not just the ones this change
    // touched: adding a line to each row must not push anything else out.
    //
    // Exception, same shape as transientTextTruncation's `button.dismiss`
    // note: Avatar.svelte's identity ring is an absolutely positioned child
    // at `inset: -2px`, so it deliberately paints 2px outside the avatar box
    // and Chromium counts that in the avatar's (and its two ancestors')
    // scrollWidth. It carries no text and is unrelated to #123; the initial
    // it wraps still gets swept, through its own `.avatar-fallback` box.
    const RING_OVERFLOW = '.avatar, .profile-button, .profile-menu-wrap';
    const overflow = [menu, ...menu.querySelectorAll('*')]
      .filter((element) => element instanceof HTMLElement)
      .filter((element) => !element.matches(RING_OVERFLOW))
      .filter((element) => element.scrollWidth > element.clientWidth)
      .map((element) => ({
        selector: selectorFor(element),
        text: (element.textContent ?? '').slice(0, 80),
        scrollWidth: element.scrollWidth,
        clientWidth: element.clientWidth
      }));

    const hero = menu.querySelector('.hero');
    const heroBounds = hero?.getBoundingClientRect();

    const measurement = {
      viewport: { width: window.innerWidth, deviceScaleFactor: window.devicePixelRatio },
      fonts: {
        status: document.fonts.status,
        mono: monoFaces.length > 0,
        display: displayFaces.length > 0
      },
      menu: {
        width: menu.getBoundingClientRect().width,
        scrollWidth: menu.scrollWidth,
        clientWidth: menu.clientWidth
      },
      documentScrollWidth: document.documentElement.scrollWidth,
      hero: heroBounds
        ? {
            height: heroBounds.height,
            scrollHeight: hero.scrollHeight,
            clientHeight: hero.clientHeight,
            overflowY: getComputedStyle(hero).overflowY
          }
        : null,
      heroTitle,
      heroCode,
      codes,
      names,
      statuses,
      overflow
    };
    document.body.dataset.roomCodeMeasurement = encodeURIComponent(JSON.stringify(measurement));
  } catch (error) {
    const message = error instanceof Error ? `${error.message}\n${error.stack ?? ''}` : String(error);
    document.body.dataset.roomCodeMeasurementError = encodeURIComponent(message);
  }
}

void renderFixture();
