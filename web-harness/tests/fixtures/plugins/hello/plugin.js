// Hand-written fixture: what `definePlugin` compiles down to, with no imports
// so the file can be packed with --no-build.
const definition = {
  activate(petal) {
    petal.log.info('hello fixture active in', petal.meeting.room().label);
    petal.ui.onAction((action) => {
      if (action.buttonId === 'hello') petal.ui.toast('Hello from a plugin').catch(() => {});
    });
    // Sandbox probe for the rendered/e2e tests: none of these may exist.
    const leaks = ['__TAURI_INTERNALS__', '__TAURI__', 'ipc'].filter((k) => k in globalThis);
    petal.log.info('sandbox-probe', JSON.stringify({ leaks, origin: String(globalThis.origin) }));
  },
};
export default (globalThis.__petalRegister ? globalThis.__petalRegister(definition) : definition);
