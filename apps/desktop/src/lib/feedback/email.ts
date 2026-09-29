// Re-export of the shared feedback reply-address check and remember helpers
// (#245) -- see shared/logic/feedbackEmail.ts for the single source of truth
// -- plus where this app remembers the last address sent: the webview's
// localStorage, the same app storage the session store uses. Cleared by a
// factory reset (`STORAGE_KEYS.feedbackEmail`).

import type { FeedbackEmailStorage } from '@petal/shared/logic/feedbackEmail';

export * from '@petal/shared/logic/feedbackEmail';

/** The webview's storage, or `null` where reading `localStorage` itself
 * throws (blocked storage) or it does not exist (no browser). */
export function feedbackEmailStorage(): FeedbackEmailStorage | null {
  try {
    return typeof localStorage === 'undefined' ? null : localStorage;
  } catch {
    return null;
  }
}
