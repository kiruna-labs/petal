// UserDispatch feedback modal gate (#292). Compile-time only, Vite-injected
// -- `import.meta.env.VITE_*` vars are baked in at build time, absent by
// default for every local/CI/contributor build (no `.env` in this repo sets
// it), same "opt-in only via an explicit build-time secret" posture as
// `PETAL_SENTRY_DSN` (src-tauri/src/logging.rs's `sentry_dsn()`) -- except
// this one must be readable from the WEBVIEW (the bundled `@userdispatch/sdk`
// runs in JS, not Rust), so it has to be a Vite `VITE_`-prefixed var rather
// than a Rust `option_env!` bake.
//
// Deliberately a PUBLIC key only: never load a hosted third-party widget
// script (rejected direction, see issue #292's resolution comment) and never
// accept anything that looks like a secret (`sk_...`) here -- this value
// ships inside the built webview bundle, which any user of the app can read.

const PUBLIC_KEY_PATTERN = /^pk_[A-Za-z0-9_-]{8,}$/;

/**
 * Pure format check, separated from the `import.meta.env` read below so it
 * is directly unit-testable without a Vite runtime (mirrors the approved
 * web-harness parity reference's `isValidUserDispatchPublicKey`, #293).
 */
export function isValidUserDispatchPublicKey(value: string | null | undefined): value is string {
  return typeof value === 'string' && PUBLIC_KEY_PATTERN.test(value.trim());
}

/**
 * The build-time UserDispatch public key, or `null` if absent/malformed.
 * `null` means the feedback feature does not exist for this build: no
 * trigger renders, no SDK module is ever imported.
 */
export function userDispatchPublicKey(): string | null {
  const raw = (import.meta.env.VITE_USERDISPATCH_PUBLIC_KEY as string | undefined)?.trim();
  return isValidUserDispatchPublicKey(raw) ? raw : null;
}

export function isFeedbackEnabled(): boolean {
  return userDispatchPublicKey() !== null;
}
