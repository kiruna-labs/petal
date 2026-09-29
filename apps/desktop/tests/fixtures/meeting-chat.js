// Mount the desktop MeetingChrome with the chat drawer open (meeting-chat.svelte).
window.__TAURI_INTERNALS__ = {
  invoke: async () => null,
  transformCallback: () => 0,
  unregisterCallback: () => {}
};

import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';

async function renderFixture() {
  try {
    const [{ mount }, { default: Fixture }] = await Promise.all([import('svelte'), import('./meeting-chat.svelte')]);
    const host = document.querySelector('#app');
    host.style.width = '100%';
    host.style.height = '100vh';
    mount(Fixture, { target: host });
    await document.fonts.ready;
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    document.body.dataset.fixtureReady = 'true';
  } catch (error) {
    document.body.dataset.fixtureError = encodeURIComponent(error instanceof Error ? error.message : String(error));
  }
}

void renderFixture();
