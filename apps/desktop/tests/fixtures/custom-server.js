// #310 fit fixture: mounts the real MainMenu (the main window's join/create
// surface) and the real Gallery (the meeting) with a custom-server line,
// then reports rendered-pixel measurements. CLAUDE.md: UI text must never
// truncate — scrollWidth <= clientWidth at the 400px main window.
import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';
import '@fontsource/albert-sans/700.css';
import { mockIPC } from '@tauri-apps/api/mocks';

function nextFrame() {
  return new Promise((resolve) => requestAnimationFrame(() => resolve()));
}

function measure(element) {
  const bounds = element.getBoundingClientRect();
  const style = getComputedStyle(element);
  return {
    text: (element.textContent ?? '').trim(),
    left: bounds.left,
    right: bounds.right,
    width: bounds.width,
    height: bounds.height,
    scrollWidth: element.scrollWidth,
    clientWidth: element.clientWidth,
    scrollHeight: element.scrollHeight,
    clientHeight: element.clientHeight,
    opacity: style.opacity,
    visibility: style.visibility,
    display: style.display,
    textOverflow: style.textOverflow,
    whiteSpace: style.whiteSpace,
    overflowWrap: style.overflowWrap
  };
}

async function renderFixture() {
  try {
    mockIPC((command) => {
      if (command === 'plugin:event|listen') return 1;
      return null;
    });

    const params = new URLSearchParams(window.location.search);
    const label = params.get('label');
    const surface = params.get('surface') === 'meeting' ? 'meeting' : 'menu';
    const [{ mount }, { default: MainMenu }, { default: Gallery }, server] = await Promise.all([
      import('svelte'),
      import('#lib/components/MainMenu.svelte'),
      import('#lib/components/Gallery.svelte'),
      import('#lib/data/customServer.svelte.js')
    ]);

    const host = document.querySelector('#app');
    const serverLabel = label ? { customServerLabel: label } : {};
    if (surface === 'meeting') {
      mount(Gallery, {
        target: host,
        props: {
          roomName: 'eng-sync',
          elapsed: '24:18',
          participants: [],
          frameless: true,
          onControl: () => {},
          ...serverLabel
        }
      });
    } else {
      mount(MainMenu, {
        target: host,
        props: {
          userName: 'Jordan Kim',
          userIdentity: 'plum',
          frameless: true,
          onCreateMeeting: () => {},
          onJoinByCode: () => {},
          ...serverLabel
        }
      });
    }

    server.noteCustomServer({
      effectiveBackendUrl: 'https://petal.example.com',
      backendSource: 'settings'
    });
    const fromSettings = server.customServerState.label;
    server.noteCustomServer({
      effectiveBackendUrl: 'https://petal.example.com:8443',
      backendSource: 'settings'
    });
    const fromSettingsPort = server.customServerState.label;
    server.noteCustomServer({
      effectiveBackendUrl: 'https://petal.example.com',
      backendSource: 'env'
    });
    const fromEnv = server.customServerState.label;
    server.noteCustomServer({
      effectiveBackendUrl: 'https://petal.example.com',
      backendSource: 'build'
    });
    const fromBuild = server.customServerState.label;
    server.noteCustomServer({
      effectiveBackendUrl: 'https://petal.example.com',
      backendSource: 'none'
    });
    const fromNone = server.customServerState.label;
    server.noteCustomServer({
      effectiveBackendUrl: null,
      backendSource: 'settings'
    });
    const fromEmpty = server.customServerState.label;

    await document.fonts.ready;
    await document.fonts.load('500 12px "Albert Sans"', label ?? 'Custom server');
    await nextFrame();
    await nextFrame();

    const indicators = [...document.querySelectorAll('[data-testid="custom-server"]')].map(measure);
    document.body.dataset.customServerMeasurement = encodeURIComponent(
      JSON.stringify({
        viewport: { width: window.innerWidth, height: window.innerHeight, deviceScaleFactor: window.devicePixelRatio },
        surface,
        fonts: document.fonts.status,
        documentScrollWidth: document.documentElement.scrollWidth,
        indicators,
        store: {
          fromSettings,
          fromSettingsPort,
          fromEnv,
          fromBuild,
          fromNone,
          fromEmpty
        }
      })
    );
  } catch (error) {
    const message = error instanceof Error ? `${error.message}\n${error.stack ?? ''}` : String(error);
    document.body.dataset.customServerMeasurementError = encodeURIComponent(message);
  }
}

void renderFixture();
