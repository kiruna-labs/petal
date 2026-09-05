// Debug mode (#669): the master switch gating the remote-window header's
// Debug button. Off by default -- it's a diagnostic affordance (frame
// counters, glass-to-glass latency, packet loss), not something most users
// need cluttering the header.
//
// The visibility predicate is a path-stability shim: the implementation
// lives in the shared package (shared/logic/debugHeaderVisibility.ts) — the
// SINGLE SOURCE OF TRUTH consumed by both this app and web-harness. Do not
// add predicate logic here; edit the shared module.
export * from '@petal/shared/logic/debugHeaderVisibility';

/** Settings-panel copy. Native-only -- web-harness has no Settings panel. */
export const DEBUG_MODE_SETTING_TITLE = 'Debug mode';
export const DEBUG_MODE_SETTING_DESCRIPTION =
  'Adds a Debug button to every remote window’s header, showing frame counters, glass-to-glass latency, and packet loss for that share. Off by default.';
