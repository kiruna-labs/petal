// #245: mounts the REAL FeedbackModal on demand, so the rendered test can
// open, close and reopen it (the remembered address is read at mount). The
// Tauri bridge is a stub -- no window is being shared, and the diagnostics
// command returns a tiny fixed archive. The UserDispatch network boundary is
// left to the test, which intercepts it.
window.__TAURI_INTERNALS__ = {
  invoke: async (command) => {
    if (command === 'shared_window_ids') return [];
    if (command === 'prepare_feedback_diagnostics') {
      return {
        filename: 'petal-feedback-diagnostics.zip',
        mimeType: 'application/zip',
        bytesBase64: btoa('PK fixture diagnostics'),
        byteCount: 22
      };
    }
    return null;
  },
  transformCallback: () => 0,
  unregisterCallback: () => {}
};

import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';
import '@fontsource/albert-sans/700.css';

let instance = null;

window.__feedbackModal = {
  closeRequests: 0,
  async open() {
    const [{ mount }, { default: FeedbackModal }] = await Promise.all([
      import('svelte'),
      import('$lib/components/FeedbackModal.svelte')
    ]);
    instance = mount(FeedbackModal, {
      target: document.querySelector('#app'),
      props: { onClose: () => (window.__feedbackModal.closeRequests += 1) }
    });
    await document.fonts.ready;
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
  },
  async close() {
    const { unmount } = await import('svelte');
    if (instance) await unmount(instance);
    instance = null;
  }
};

document.body.dataset.ready = 'true';
