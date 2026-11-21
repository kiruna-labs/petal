// #487 fit fixture: mounts the real LiveStreamBanner (the meeting window's
// LIVE notice) in its longest states and reports rendered sizes. CLAUDE.md:
// UI text must never truncate, at the 400px main window and its 380px minimum.
import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';
import '@fontsource/albert-sans/700.css';
import { mockIPC } from '@tauri-apps/api/mocks';
import { tick } from 'svelte';

function nextFrame() {
  return new Promise((resolve) => requestAnimationFrame(() => resolve()));
}

function measure(element) {
  const bounds = element.getBoundingClientRect();
  return {
    tag: element.tagName.toLowerCase(),
    className: String(element.className),
    text: (element.textContent ?? '').trim().replace(/\s+/g, ' '),
    left: bounds.left,
    right: bounds.right,
    scrollWidth: element.scrollWidth,
    clientWidth: element.clientWidth,
    textOverflow: getComputedStyle(element).textOverflow
  };
}

const HOUR = 3_600_000;

function snapshot(phase) {
  return {
    phase,
    windowId: 7,
    startedAt: Date.now() - 12 * HOUR,
    width: 3840,
    height: 2160,
    fps: 60,
    targetBitrateBps: 50_000_000,
    actualBitrateBps: 49_900_000,
    droppedFrames: 123_456,
    slateRemainingS: 120,
    reconnectAttempt: 3,
    hardwareEncoder: true,
    noAudioOutput: true
  };
}

async function renderFixture() {
  try {
    const scenario = new URLSearchParams(window.location.search).get('scenario') ?? 'live';
    const own = scenario === 'optOut' || scenario === 'roomPaused' ? null : snapshot(scenario);
    mockIPC((command) => {
      switch (command) {
        case 'plugin:event|listen':
          return 1;
        case 'stream_live_state':
          return {
            live: true,
            ownerIdentity: 'someone',
            windowId: 7,
            paused: scenario === 'roomPaused',
            startedAt: Date.now() - 12 * HOUR,
            streams: []
          };
        case 'stream_settings':
          return { alwaysStreamerMode: false, keepOffLiveStreams: false };
        case 'stream_session_state':
          return own;
        default:
          return null;
      }
    });
    const [{ mount }, { default: Banner }] = await Promise.all([
      import('svelte'),
      import('#lib/components/LiveStreamBanner.svelte')
    ]);
    mount(Banner, { target: document.querySelector('#app') });
    for (let i = 0; i < 5; i += 1) {
      await tick();
      await nextFrame();
    }
    await document.fonts.ready;
    await document.fonts.load('600 12px "Albert Sans"', 'LIVE');
    await nextFrame();
    const banner = document.querySelector('.live-banner');
    if (!banner) throw new Error('the banner did not render');
    document.body.dataset.bannerMeasurement = encodeURIComponent(
      JSON.stringify({
        viewport: { width: window.innerWidth, deviceScaleFactor: window.devicePixelRatio },
        fonts: document.fonts.status,
        documentScrollWidth: document.documentElement.scrollWidth,
        banner: measure(banner),
        texts: [...banner.querySelectorAll('.text, button, label')].map(measure)
      })
    );
  } catch (error) {
    document.body.dataset.bannerMeasurementError = encodeURIComponent(String(error?.stack ?? error));
  }
}

void renderFixture();
