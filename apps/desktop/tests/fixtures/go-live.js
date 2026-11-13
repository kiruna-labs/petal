// #487 fit fixture: mounts the real go-live dialog page (its own window,
// 400px minimum) with every hint it can show, optionally on a plain rtmp://
// URL (new or saved), and reports each text element's rendered size.
// CLAUDE.md: UI text must never truncate.
import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';
import '@fontsource/albert-sans/700.css';
import '@fontsource/jetbrains-mono/400.css';
import { mockIPC } from '@tauri-apps/api/mocks';
import { tick } from 'svelte';

function nextFrame() {
  return new Promise((resolve) => requestAnimationFrame(() => resolve()));
}

function measure(element) {
  const bounds = element.getBoundingClientRect();
  const style = getComputedStyle(element);
  return {
    tag: element.tagName.toLowerCase(),
    className: element.className,
    text: (element.textContent ?? '').trim().replace(/\s+/g, ' '),
    left: bounds.left,
    right: bounds.right,
    width: bounds.width,
    height: bounds.height,
    scrollWidth: element.scrollWidth,
    clientWidth: element.clientWidth,
    scrollHeight: element.scrollHeight,
    clientHeight: element.clientHeight,
    textOverflow: style.textOverflow,
    visibility: style.visibility,
    display: style.display,
    opacity: style.opacity,
    color: style.color,
    background: style.backgroundColor
  };
}

/** A select shows its chosen option's text: does that fit beside the arrow? */
function measureSelect(select) {
  const style = getComputedStyle(select);
  const context = document.createElement('canvas').getContext('2d');
  context.font = `${style.fontWeight} ${style.fontSize} ${style.fontFamily}`;
  const text = select.selectedOptions[0]?.textContent ?? '';
  const room =
    select.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight) - SELECT_ARROW_PX;
  return { text, textWidth: context.measureText(text).width, room };
}

/** Room a native select keeps for its drop-down arrow. */
const SELECT_ARROW_PX = 24;

function warnings() {
  return [...document.querySelectorAll('main .hint.warning')].map(measure);
}

const PREVIEW = {
  captureWidth: 3840,
  captureHeight: 2160,
  shareFps: 60,
  plan: { width: 1920, height: 1080, fps: 30, bitrateBps: 6_000_000, maxBitrateBps: 6_000_000, resolution: '1080p' },
  clientAvailable: true,
  clientEncoder: 'cpu',
  audioOutput: false
};

async function renderFixture() {
  try {
    const params = new URLSearchParams(window.location.search);
    const scenario = params.get('scenario') ?? 'new';
    const streamerMode = params.get('streamerMode') === '1';
    const url = params.get('url') ?? 'rtmp://live.example.com/app';
    const saved =
      scenario === 'saved'
        ? [{ id: 'd1', name: 'Team channel (backup ingest)', serverUrl: url }]
        : [];
    mockIPC((command) => {
      switch (command) {
        case 'plugin:event|listen':
          return 1;
        case 'go_live_target':
          return 7;
        case 'stream_destinations_list':
          return saved;
        case 'stream_preview':
          return PREVIEW;
        case 'streamer_mode_state':
          return { active: streamerMode, live: false };
        default:
          return null;
      }
    });

    const [{ mount }, { default: GoLive }] = await Promise.all([
      import('svelte'),
      import('../../src/routes/go-live/+page.svelte')
    ]);
    mount(GoLive, { target: document.querySelector('#app') });
    for (let i = 0; i < 5; i += 1) {
      await tick();
      await nextFrame();
    }

    if (scenario === 'new' || scenario === 'help') {
      // `help`: the longest service name, for "Where do I find my … key?".
      const service = document.querySelectorAll('main select')[1];
      service.value = scenario === 'help' ? 'facebook' : 'custom';
      service.dispatchEvent(new Event('change', { bubbles: true }));
      await tick();
      if (scenario === 'new') {
        const input = document.querySelector('main input[placeholder^="rtmps://"]');
        input.value = url;
        input.dispatchEvent(new Event('input', { bubbles: true }));
      }
      // A long bitrate hint too.
      const bitrate = document.querySelector('main input[inputmode="decimal"]');
      bitrate.value = '500';
      bitrate.dispatchEvent(new Event('input', { bubbles: true }));
    }
    for (let i = 0; i < 3; i += 1) {
      await tick();
      await nextFrame();
    }

    await document.fonts.ready;
    // Measured now; then the warning must go when the choice changes.
    const shown = warnings();
    const urlInputType = document.querySelector('main input[placeholder^="rtmps://"]')?.type ?? null;
    await document.fonts.load('400 13px "Albert Sans"', 'Go live');
    await nextFrame();

    const main = document.querySelector('main');
    const texts = [
      ...main.querySelectorAll('h1, p, legend, label, button, .segmented label')
    ].map(measure);
    const selects = [...main.querySelectorAll('select')].map(measureSelect);
    const documentScrollWidth = document.documentElement.scrollWidth;
    const mainMeasure = measure(main);

    // Changing the choice: a new destination back to an rtmps:// preset, a
    // saved one to "New destination…" (YouTube's rtmps:// preset).
    if (scenario === 'new') {
      const service = document.querySelectorAll('main select')[1];
      service.value = 'youtube';
      service.dispatchEvent(new Event('change', { bubbles: true }));
    } else if (scenario === 'saved') {
      const destination = document.querySelectorAll('main select')[0];
      destination.value = '__new__';
      destination.dispatchEvent(new Event('change', { bubbles: true }));
    }
    for (let i = 0; i < 3; i += 1) {
      await tick();
      await nextFrame();
    }
    const afterChange = warnings().map((warning) => warning.text);
    document.body.dataset.goLiveMeasurement = encodeURIComponent(
      JSON.stringify({
        viewport: { width: window.innerWidth, deviceScaleFactor: window.devicePixelRatio },
        fonts: document.fonts.status,
        documentScrollWidth,
        main: mainMeasure,
        texts,
        selects,
        warnings: shown,
        afterChange,
        urlInputType
      })
    );
  } catch (error) {
    document.body.dataset.goLiveMeasurementError = encodeURIComponent(String(error?.stack ?? error));
  }
}

void renderFixture();
