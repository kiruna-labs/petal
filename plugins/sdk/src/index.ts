// Plugin-author facing entry point. Runtime footprint is one function: the
// frame runtime (shared/plugin-host/frameRuntime.ts) installs
// `window.__petalRegister` before the plugin module runs; `definePlugin`
// hands the definition over. Everything else here is types.
//
// Types are re-exported from shared/plugin-host so there is exactly one
// definition of the API; the relative paths are a workspace convenience and
// get bundled away when this package is published.

export type {
  Petal,
  PluginDefinition,
  SurfaceContext,
  SurfaceChannel,
  Participant,
  RoomInfo,
  SharedWindow,
  DataMessage,
  PublishOptions,
  UiAction,
  ButtonPatch,
  FetchInit,
  FetchResult,
  Json,
  Unsubscribe,
  MeetingPhase,
  LogLevel,
} from '../../../shared/plugin-host/api.ts';

export type {
  PluginManifest,
  Permission,
  PluginScope,
  SurfaceKind,
  PluginContributions,
  ToolbarButtonContribution,
  HeaderButtonContribution,
  SurfaceContribution,
  SettingsFieldContribution,
} from '../../../shared/plugin-host/manifest.ts';

import type { PluginDefinition } from '../../../shared/plugin-host/api.ts';

declare global {
  interface Window {
    __petalRegister?: (definition: PluginDefinition) => PluginDefinition;
  }
}

/**
 * Register the plugin with the host. Call it exactly once at module top
 * level and export the result as default so tooling can find it.
 */
export function definePlugin(definition: PluginDefinition): PluginDefinition {
  if (typeof window !== 'undefined' && typeof window.__petalRegister === 'function') {
    return window.__petalRegister(definition);
  }
  // Outside a Petal frame (unit tests, SSR): a no-op so the module still evaluates.
  return definition;
}
