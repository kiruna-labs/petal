import * as Sentry from '@sentry/browser';
import type { LogKind } from './ui/logging';
import { sensitiveStringRegistry, type SensitiveStringRegistry } from './sensitiveStrings';

// ---------------------------------------------------------------------------
// Sentry error reporting (#283). Gated on VITE_SENTRY_DSN being set at build
// time (mirrors the VITE_PETAL_BACKEND_URL convention in connection.ts /
// vercel.json) -- unset in local dev = fully off, no SDK initialization call
// happens at all.
//
// Error capture only. Do NOT opt into the SDK's screen-recording/session
// replay product or its browser-tracing/performance product below (grep the
// Sentry SDK's exported integration factories before adding anything to this
// file), and do not raise `tracesSampleRate` above 0 -- this app's UI shows
// room names, participant lists, and webcam thumbnails on screen, and
// DOM/canvas recording is a realistic PII/content leak path. The only
// integrations here are Sentry's own defaults (which include the global
// window.onerror / unhandledrejection handlers), so no screen-recording or
// tracing/performance product is ever enabled, regardless of DSN.
//
// PII: every breadcrumb/event message and exception value is passed through
// `sensitiveStringRegistry.scrub()` (see sensitiveStrings.ts) before Sentry
// can send it. `sendDefaultPii` is explicitly disabled rather than relying
// on the SDK default.
// ---------------------------------------------------------------------------

type ViteImportMeta = ImportMeta & {
  env?: {
    VITE_SENTRY_DSN?: string;
  };
};

const MAX_BREADCRUMBS = 50;

export function sentryDsn(): string | undefined {
  const env = (import.meta as ViteImportMeta).env;
  return env?.VITE_SENTRY_DSN?.trim() || undefined;
}

let initialized = false;

export function initSentry(
  logEvent: (message: string, kind?: LogKind) => void,
  registry: SensitiveStringRegistry = sensitiveStringRegistry
): boolean {
  const dsn = sentryDsn();
  if (!dsn) return false;
  if (initialized) return true;

  Sentry.init({
    dsn,
    tracesSampleRate: 0,
    sendDefaultPii: false,
    maxBreadcrumbs: MAX_BREADCRUMBS,
    beforeBreadcrumb: (breadcrumb) => scrubBreadcrumb(breadcrumb, registry),
    beforeSend: (event) => scrubEvent(event, registry),
  });
  initialized = true;
  logEvent('Sentry error reporting initialized', 'info');
  return true;
}

/** Test-only: allow a fresh module-level `initialized` flag between tests. */
export function resetSentryInitializedForTests(): void {
  initialized = false;
}

/**
 * Forwards a local `logEvent()` call into Sentry as a breadcrumb, so a
 * captured error's Sentry report carries the same recent-activity trail the
 * session log shows locally (#283's "makes the breadcrumb trail actually
 * useful"). No-op when Sentry was never initialized (no DSN configured) --
 * matches the "absent-by-default" requirement, no Sentry API surface touched
 * at all for local dev. The raw `message` may contain room/participant text;
 * it is scrubbed by `beforeBreadcrumb` (see `initSentry` above) before it can
 * leave the browser -- this function must never bypass that hook.
 */
export function addSentryBreadcrumb(message: string, kind: LogKind = 'info'): void {
  if (!initialized) return;
  Sentry.addBreadcrumb({
    category: 'session-log',
    level: sentryLevelForLogKind(kind),
    message,
  });
}

function sentryLevelForLogKind(kind: LogKind): Sentry.SeverityLevel {
  switch (kind) {
    case 'error':
      return 'error';
    case 'warn':
      return 'warning';
    case 'ok':
    case 'info':
    default:
      return 'info';
  }
}

export function scrubBreadcrumb(breadcrumb: Sentry.Breadcrumb, registry: SensitiveStringRegistry): Sentry.Breadcrumb {
  const scrubbed: Sentry.Breadcrumb = { ...breadcrumb };
  if (typeof scrubbed.message === 'string') {
    scrubbed.message = registry.scrub(scrubbed.message);
  }
  if (scrubbed.data && typeof scrubbed.data === 'object') {
    scrubbed.data = scrubStringFields(scrubbed.data as Record<string, unknown>, registry);
  }
  return scrubbed;
}

export function scrubEvent(event: Sentry.ErrorEvent, registry: SensitiveStringRegistry): Sentry.ErrorEvent {
  const scrubbed: Sentry.ErrorEvent = { ...event };
  if (typeof scrubbed.message === 'string') {
    scrubbed.message = registry.scrub(scrubbed.message);
  }
  if (scrubbed.exception?.values) {
    scrubbed.exception = {
      ...scrubbed.exception,
      values: scrubbed.exception.values.map((value) => ({
        ...value,
        value: typeof value.value === 'string' ? registry.scrub(value.value) : value.value,
      })),
    };
  }
  return scrubbed;
}

function scrubStringFields(
  data: Record<string, unknown>,
  registry: SensitiveStringRegistry
): Record<string, unknown> {
  const result: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(data)) {
    result[key] = typeof value === 'string' ? registry.scrub(value) : value;
  }
  return result;
}

/**
 * Mirrors uncaught errors/unhandled rejections into the local session log,
 * independent of whether Sentry is configured for this build. Sentry's own
 * global handlers (via its default integrations) capture the same events
 * separately when a DSN is set.
 */
export function installGlobalErrorMirror(logEvent: (message: string, kind?: LogKind) => void): void {
  window.addEventListener('error', (event: ErrorEvent) => {
    const detail = event.error instanceof Error ? event.error.message : event.message;
    logEvent(`uncaught error: ${detail}`, 'error');
  });
  window.addEventListener('unhandledrejection', (event: PromiseRejectionEvent) => {
    const reason = event.reason instanceof Error ? event.reason.message : String(event.reason);
    logEvent(`unhandled promise rejection: ${reason}`, 'error');
  });
}
