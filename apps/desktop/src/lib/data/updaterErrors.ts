// Maps a raw updater error (from `updater.ts`'s catch block) to a short,
// fixed-length, human summary for the UI, plus whether the user has any way
// out of it. The raw message can be an arbitrarily long, technical
// OS/library string -- e.g. a full temp-file path from a failed archive
// unpack -- which is exactly right for petal.log, but showing it verbatim in
// a toast produced a broken, overflowing layout (a 6-line pill with a raw
// /var/folders/... path spilling past the window edge, see #105). The full
// raw text is never lost; it's still logged in full by `updater.ts`'s own
// logUpdaterStep call alongside this. No `$lib` imports here (mirrors
// shareErrors.ts) so this stays directly unit-testable under plain
// `node --test`.

/** Mirrors `src-tauri/src/updater.rs`'s `INCOMPATIBLE_UPDATE_MARKER`. The
 *  Rust guard stamps it on EVERY rejected update archive; change both sides
 *  together (#125). */
export const INCOMPATIBLE_UPDATE_MARKER = 'update is incompatible with';

export type UpdateFailureCategory =
  | 'restore-previous'
  | 'move-to-applications'
  | 'needs-admin'
  | 'incompatible'
  | 'install'
  | 'signature'
  | 'network'
  | 'unknown';

/**
 * Classify a raw updater error.
 *
 * Order matters. `incompatible` is tested FIRST because the Rust guard's own
 * detail text ("update archive is not a Windows executable") contains the
 * word "archive" and would otherwise be reported as a retryable install
 * failure -- which is exactly the dead end #125 exists to remove.
 */
export function updateFailureCategory(raw: string): UpdateFailureCategory {
  const lower = raw.toLowerCase();
  // The install-failure cases from `updater.rs`'s macOS installer (#871).
  // These come first because the raw text carries paths that would otherwise
  // fall through to a generic "see logs" the user cannot act on. The marker
  // phrases are the stable contract with `mac_install_user_message`; the
  // path-bearing detail stays in petal.log, exactly as #105 requires.
  if (lower.includes('previous petal is safe')) return 'restore-previous';
  if (lower.includes('read-only disk image') || lower.includes('different disks')) {
    return 'move-to-applications';
  }
  if (lower.includes('administrator password')) return 'needs-admin';
  if (lower.includes(INCOMPATIBLE_UPDATE_MARKER)) return 'incompatible';
  if (lower.includes('unpack') || lower.includes('extract') || lower.includes('archive')) {
    return 'install';
  }
  if (lower.includes('signature') || lower.includes('verify')) return 'signature';
  if (lower.includes('architecture') || lower.includes('not supported')) return 'incompatible';
  if (
    lower.includes('network') ||
    lower.includes('fetch') ||
    lower.includes('dns') ||
    lower.includes('timeout') ||
    lower.includes('connection')
  ) {
    return 'network';
  }
  return 'unknown';
}

export function friendlyUpdateErrorMessage(raw: string): string {
  switch (updateFailureCategory(raw)) {
    case 'restore-previous':
      return 'see the logs to restore your previous Petal';
    case 'move-to-applications':
      return 'move Petal to Applications, then update';
    case 'needs-admin':
      return 'an administrator password is needed';
    case 'incompatible':
      // Device-neutral on purpose: this exact sentence used to say "your
      // Mac" on Windows too, which is the wrong-device wording #116 fixed on
      // the Rust side and missed here.
      return "This update isn't compatible with this device";
    case 'install':
      return "Couldn't install the update — try again later";
    case 'signature':
      return 'Update failed a security check and was rejected';
    case 'network':
      return "Couldn't reach the update server — check your connection";
    case 'unknown':
      return 'Update check failed — see logs for details';
  }
}

/**
 * True when the only remaining way out is a manual reinstall (#125).
 *
 * Every Windows install from v0.8.5 to 0.9.14 shipped a guard that rejected
 * its own update archive, so those clients are permanently pinned and the
 * in-app updater can never rescue them. A retry loop is not an answer; a
 * download link is. Every other failure category is transient or already
 * carries its own instruction, so none of them get the action.
 */
export function updateFailureOffersManualDownload(raw: string): boolean {
  return updateFailureCategory(raw) === 'incompatible';
}
