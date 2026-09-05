// In-process updater status for the main webview toast host.
//
// This deliberately stays frontend-local: `checkForUpdate()` already runs in
// this webview on startup, so routing these transient states through a Tauri
// event would add latency and failure surface without crossing a process
// boundary.

export type UpdateStatus =
  | { kind: 'idle' }
  | { kind: 'downloading' }
  | { kind: 'relaunching' }
  | { kind: 'available'; version?: string }
  | { kind: 'pending-relaunch'; version?: string }
  | { kind: 'failed'; message: string };

export const updateStatus = $state<UpdateStatus>({ kind: 'idle' });

export function clearUpdateStatus() {
  updateStatus.kind = 'idle';
}

export function markUpdateDownloading() {
  updateStatus.kind = 'downloading';
}

export function markUpdateRelaunching() {
  updateStatus.kind = 'relaunching';
}

export function markUpdateAvailable(version?: string) {
  Object.assign(updateStatus, { kind: 'available' as const, version });
}

export function markUpdatePendingRelaunch(version?: string) {
  Object.assign(updateStatus, { kind: 'pending-relaunch' as const, version });
}

export function markUpdateFailed(message: string) {
  Object.assign(updateStatus, { kind: 'failed' as const, message });
}
