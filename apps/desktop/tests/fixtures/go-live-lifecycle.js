// #487 lifecycle fixture: mounts the real go-live dialog page with Tauri's IPC
// and events mocked, then drives its real handlers the way the native side
// and the user do: a failed go-live, Cancel, the `go-live-target` event that
// brings a hidden (macOS) dialog back for another share, and a go-live that
// succeeds. Records every command the page invokes, in order.
import { mockIPC, mockWindows } from '@tauri-apps/api/mocks';
import { emit } from '@tauri-apps/api/event';
import { tick } from 'svelte';

const PREVIEW = {
  captureWidth: 1920,
  captureHeight: 1080,
  shareFps: 60,
  plan: { width: 1920, height: 1080, fps: 30, bitrateBps: 6_000_000, maxBitrateBps: 6_000_000, resolution: '1080p' },
  clientAvailable: true,
  clientEncoder: 'cpu',
  audioOutput: true
};

async function settle() {
  for (let i = 0; i < 6; i += 1) {
    await tick();
    await new Promise((resolve) => requestAnimationFrame(() => resolve()));
  }
}

function setValue(element, value, event = 'input') {
  element.value = value;
  element.dispatchEvent(new Event(event, { bubbles: true }));
}

function button(text) {
  return [...document.querySelectorAll('main button')].find((b) => b.textContent.trim() === text);
}

async function run() {
  const calls = [];
  const steps = {};
  let goLiveFails = true;
  mockIPC(
    (command, args) => {
      calls.push({ command, windowId: args?.windowId ?? args?.request?.windowId ?? null });
      switch (command) {
        case 'go_live_target':
          return 7;
        case 'stream_destinations_list':
          return [];
        case 'stream_preview':
          return PREVIEW;
        case 'streamer_mode_state':
          return { active: false, live: false };
        case 'stream_go_live':
          if (goLiveFails) throw 'The server refused the stream key';
          return { phase: 'connecting', windowId: args.request.windowId };
        default:
          return null;
      }
    },
    { shouldMockEvents: true }
  );
  // The dialog's own window (`GO_LIVE_WINDOW_LABEL`), so getCurrentWindow() works.
  mockWindows('go-live');
  const [{ mount }, { default: GoLive }] = await Promise.all([
    import('svelte'),
    import('../../src/routes/go-live/+page.svelte')
  ]);
  mount(GoLive, { target: document.querySelector('#app') });
  await settle();
  const keyInput = () => document.querySelector('main input[type="password"]');
  const errorText = () => document.querySelector('main [role="alert"]')?.textContent.trim() ?? null;
  const since = (n) => calls.slice(n).map((c) => c.command);
  steps.opened = { calls: calls.map((c) => ({ ...c })) };

  // A new destination, then a go-live the server refuses: the dialog stays.
  setValue(document.querySelectorAll('main select')[1], 'custom', 'change');
  await settle();
  setValue(document.querySelector('main input[placeholder^="rtmps://"]'), 'rtmps://live.example.com/app');
  setValue(keyInput(), 'secret-key');
  await settle();
  let mark = calls.length;
  button('Go live').click();
  await settle();
  steps.failedGoLive = { calls: since(mark), error: errorText(), key: keyInput()?.value ?? null };

  // Cancel: the page closes its window and drops the key.
  mark = calls.length;
  button('Cancel').click();
  await settle();
  steps.cancel = { calls: since(mark), key: keyInput()?.value ?? null };

  // Typed a key again, then closed by the title bar, not Cancel: macOS only
  // hides the window, and the native close handler tells the page first.
  setValue(keyInput(), 'typed-then-closed');
  await settle();
  steps.typedBeforeClose = keyInput()?.value ?? null;
  await emit('go-live-closed');
  await settle();
  steps.closedByTitleBar = { key: keyInput()?.value ?? null };

  // Reopened for share 9 (macOS reuses the hidden window): the native side
  // emits `go-live-target`; nothing from the last open may linger.
  mark = calls.length;
  await emit('go-live-target', 9);
  await settle();
  steps.reopened = {
    calls: calls.slice(mark).map((c) => ({ ...c })),
    error: errorText(),
    key: keyInput()?.value ?? null
  };

  // Go live for real: the request names share 9, then the dialog retires.
  goLiveFails = false;
  setValue(keyInput(), 'secret-key');
  await settle();
  mark = calls.length;
  button('Go live').click();
  await settle();
  steps.goLive = { calls: calls.slice(mark).map((c) => ({ ...c })), key: keyInput()?.value ?? null, error: errorText() };

  document.body.dataset.lifecycle = encodeURIComponent(JSON.stringify(steps));
}

run().catch((error) => {
  document.body.dataset.lifecycleError = encodeURIComponent(String(error?.stack ?? error));
});
