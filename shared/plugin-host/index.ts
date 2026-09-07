// Public surface of the shared plugin host. Both clients import from here.
// (builtins.ts is deliberately NOT re-exported: it carries Vite `?raw`
// imports and must be imported directly by app code only.)
export * from './manifest.ts';
export * from './api.ts';
export * from './protocol.ts';
export * from './permissions.ts';
export * from './rateLimit.ts';
export * from './frame.ts';
export { FRAME_RUNTIME_SOURCE } from './frameRuntime.ts';
export * from './broker.ts';
export * from './surfaces.ts';
export * from './settingsModel.ts';
export * from './icons.ts';
export * from './host.ts';
