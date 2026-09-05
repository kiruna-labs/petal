// Main-window traffic-light actions (red = hide, yellow = minimize).
//
// Red must NEVER be close() or quit_app: close() destroys the webview and no
// builder anywhere can recreate label `main`, and quit_app marks quitting,
// leaves the room and exits the process. Hiding is recoverable -- see lib.rs's
// RunEvent::Reopen handler and the menubar "Open Petal" row.
//
// Dependency-injected so the behaviour is testable without a Tauri bridge.

export interface MainWindowControlDeps {
  hide: () => Promise<void>;
  minimize: () => Promise<void>;
}

export async function hideMainWindow(deps: MainWindowControlDeps): Promise<void> {
  try {
    await deps.hide();
  } catch (e) {
    console.error('hide main window failed', e);
  }
}

export async function minimizeMainWindow(deps: MainWindowControlDeps): Promise<void> {
  try {
    await deps.minimize();
  } catch (e) {
    console.error('minimize main window failed', e);
  }
}
