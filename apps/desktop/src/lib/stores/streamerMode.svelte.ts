// Streamer mode, as this webview sees it. Rust owns the state
// (`streaming::streamer_mode`): it is on while this computer streams, or
// always when Settings → Live streaming says so. While it is on, meeting
// names, codes and links are masked; the masking helpers are in
// `data/liveStream.ts`. A window that masks anything calls
// `watchStreamerMode()` once and reads `streamerMode.active`.

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { COMMANDS, EVENTS, hasTauriBridge, type StreamerModeState } from '../ipc.ts';

export const streamerMode = $state<StreamerModeState>({ active: false, live: false });

let watchers = 0;
let unlisten: Promise<() => void> | null = null;

/** Follow streamer mode; returns the matching unsubscribe. */
export function watchStreamerMode(): () => void {
  if (!hasTauriBridge()) return () => {};
  watchers += 1;
  if (watchers === 1) {
    unlisten = listen<StreamerModeState>(EVENTS.streamerModeChanged, (event) => {
      Object.assign(streamerMode, event.payload);
    });
    void invoke<StreamerModeState>(COMMANDS.streamerModeState)
      .then((state) => Object.assign(streamerMode, state))
      .catch(() => {});
  }
  return () => {
    watchers -= 1;
    if (watchers === 0) {
      unlisten?.then((u) => u()).catch(() => {});
      unlisten = null;
    }
  };
}
