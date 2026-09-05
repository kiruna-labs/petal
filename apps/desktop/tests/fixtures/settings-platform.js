import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';
import '@fontsource/albert-sans/700.css';

// Deliberately NO mockIPC: the fixture runs browser-only, so no Tauri command
// is ever invoked — Settings renders with its plain-browser fallbacks and the
// platform gates evaluate against the real navigator.userAgent (which the
// test overrides per case).

// Settings' mount effect acquires a real camera preview; in headless
// single-process Chromium the capture stack can abort the whole test browser
// with a GPU fatal failure. The fixture's point is rendered copy, not media,
// so reject like a machine with no camera — Settings' own no-camera fallback
// path handles NotFoundError identically to a real headless environment.
if (navigator.mediaDevices) {
  navigator.mediaDevices.getUserMedia = () =>
    Promise.reject(new DOMException('no camera in headless fixture', 'NotFoundError'));
}

async function renderFixture() {
  try {
    const [{ mount }, { default: Settings }] = await Promise.all([
      import('svelte'),
      import('$lib/components/Settings.svelte')
    ]);
    mount(Settings, { target: document.querySelector('#app') });
    await document.fonts.ready;
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    if (!document.querySelector('.section')) throw new Error('Settings sections did not render');
    // Open the reset confirm block (pure state flip, no Tauri invoke) so the
    // macOS-only tccutil instructions and the "Reset and quit" button are in
    // the rendered text the test asserts on.
    const resetButton = document.querySelector('button.reset-button.danger');
    if (resetButton instanceof HTMLButtonElement) resetButton.click();
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    document.body.dataset.settingsRendered = '1';
  } catch (error) {
    document.body.dataset.settingsRenderedError = encodeURIComponent(
      error instanceof Error ? error.message : String(error)
    );
  }
}

void renderFixture();
