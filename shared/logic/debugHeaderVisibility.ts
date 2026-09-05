// SINGLE SOURCE OF TRUTH for whether a remote window header's Debug control
// may render (#669). Consumed by the desktop app
// (apps/desktop/src/lib/data/debugMode.ts re-exports this, mirroring how
// localEcho.ts is re-exported there) and the web client
// (web-harness/src/remoteWindowHeader.ts imports it directly) so the two can
// never independently drift on when the button disappears.
//
// Debug is a diagnostic affordance (frame counters, glass-to-glass latency,
// packet loss), gated on three independent things -- ALL must hold:
// 1. the user's own "Debug mode" setting (default OFF, #669). Rust-owned on
//    desktop, not localStorage -- each Tauri webview is its own JS realm, so
//    a Settings-window toggle stored in localStorage would never reach an
//    already-open remote-window webview. The setter emits a change event so
//    an already-open header picks it up live (`set_debug_mode` in
//    `debug_settings.rs`) rather than only on next reopen.
// 2. the header is not currently showing the AI chat live disclosure -- that
//    third-party-data-sharing notice outranks a diagnostic affordance, the
//    same reason Open URL already yields to it (see
//    RemoteWindowHeader.svelte's `.header.ai-chat-live` rule). Only the
//    desktop client currently measures/enforces this; a caller with no such
//    concept passes `false`.
// 3. the header is wide enough to hold the control at all (mirrors the
//    desktop client's existing, separately measured `@media (max-width:
//    640px)` / web client's `@container (max-width: 640px)` rules). Both
//    clients already enforce this suppression in CSS; `viewportWidth` is
//    exposed here mainly so the composition is fully unit-testable, and
//    production callers that don't independently track live width may pass
//    `Number.POSITIVE_INFINITY` to defer entirely to their existing,
//    already-measured CSS breakpoint rather than duplicating it in JS.

/** Below this header/tile width the Debug control never fits (px). */
export const DEBUG_HEADER_MIN_WIDTH = 640;

export interface DebugHeaderVisibilityInput {
  /** The user's "Debug mode" master switch (default off). */
  debugModeEnabled: boolean;
  /** True while this header is showing the AI chat live disclosure. */
  aiChatLive: boolean;
  /** Current header/tile width in px. */
  viewportWidth: number;
}

/**
 * Whether the Debug control may appear on a remote window's header.
 *
 * Pure. An enabled setting still yields to both layout suppressors -- a live
 * third-party disclosure and a header too narrow to hold the control.
 */
export function debugHeaderControlVisible(input: DebugHeaderVisibilityInput): boolean {
  return (
    input.debugModeEnabled &&
    !input.aiChatLive &&
    input.viewportWidth > DEBUG_HEADER_MIN_WIDTH
  );
}
