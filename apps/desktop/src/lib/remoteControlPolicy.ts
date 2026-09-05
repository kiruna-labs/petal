import type { RemoteControlPolicy } from './ipc.ts';

/**
 * Consent-flow migration for the persisted session: a session saved with the
 * old boolean `allowRemoteControlByDefault` maps `true -> ask` (NOT auto --
 * consent is the new default for everyone) and `false -> off`; a session
 * that already carries a policy keeps it; a fresh install is `ask`. Unknown
 * strings fall back to `ask`, never to `auto`. Plain module (no runes, no
 * `$app`) so node tests can import it directly.
 */
export function migrateRemoteControlPolicy(parsed: {
  remoteControlPolicy?: unknown;
  allowRemoteControlByDefault?: unknown;
}): RemoteControlPolicy {
  const policy = parsed?.remoteControlPolicy;
  if (policy === 'off' || policy === 'ask' || policy === 'auto') return policy;
  if (parsed?.allowRemoteControlByDefault === false) return 'off';
  return 'ask';
}
