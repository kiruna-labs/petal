import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';
import '@fontsource/albert-sans/700.css';
import { platformKey } from '$lib/platform';

document.documentElement.dataset.platform = platformKey();

// Deliberately NO mockIPC: the fixture runs browser-only, so no Tauri command
// is ever invoked — MainMenu renders with its plain-browser fallbacks (room
// list stays empty, so the create button takes its default green state) and
// the platform gates evaluate against the real navigator.userAgent (which
// the test overrides per case).

async function renderFixture() {
  try {
    const [{ mount }, { default: MainMenu }] = await Promise.all([
      import('svelte'),
      import('$lib/components/MainMenu.svelte')
    ]);
    // userName is a required prop (the real /main route wires it from the
    // session store); the create/join controls render only when at least one
    // action handler is wired (the real /main route passes both).
    mount(MainMenu, {
      target: document.querySelector('#app'),
      props: {
        userName: 'Guest',
        onCreateMeeting: () => {},
        onJoinByCode: () => {},
        onOpenSettings: () => {}
      }
    });
    await document.fonts.ready;
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    if (!document.querySelector('.join-input')) throw new Error('MainMenu did not render');
    document.body.dataset.mainRendered = '1';
  } catch (error) {
    document.body.dataset.mainRenderedError = encodeURIComponent(
      error instanceof Error ? `${error.message}\n${error.stack}` : String(error)
    );
  }
}

void renderFixture();
