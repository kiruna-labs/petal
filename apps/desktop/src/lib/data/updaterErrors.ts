// Maps a raw updater error (from `updater.ts`'s catch block) to a short,
// fixed-length, human summary for the UI. The raw message can be an
// arbitrarily long, technical OS/library string -- e.g. a full temp-file
// path from a failed archive unpack -- which is exactly right for
// petal.log, but showing it verbatim in a toast produced a broken,
// overflowing layout (a 6-line pill with a raw /var/folders/... path
// spilling past the window edge, see #105). The full raw text is never
// lost; it's still logged in full by `updater.ts`'s own logUpdaterStep call
// alongside this. No `$lib` imports here (mirrors shareErrors.ts) so this
// stays directly unit-testable under plain `node --test`.
export function friendlyUpdateErrorMessage(raw: string): string {
  const lower = raw.toLowerCase();
  // The install-failure cases from `updater.rs`'s macOS installer (#871).
  // These come first because the raw text carries paths that would otherwise
  // fall through to a generic "see logs" the user cannot act on. The marker
  // phrases are the stable contract with `mac_install_user_message`; the
  // path-bearing detail stays in petal.log, exactly as #105 requires.
  if (lower.includes('previous petal is safe')) {
    return 'see the logs to restore your previous Petal';
  }
  if (lower.includes('read-only disk image') || lower.includes('different disks')) {
    return 'move Petal to Applications, then update';
  }
  if (lower.includes('administrator password')) {
    return 'an administrator password is needed';
  }
  if (lower.includes('unpack') || lower.includes('extract') || lower.includes('archive')) {
    return "Couldn't install the update — try again later";
  }
  if (lower.includes('signature') || lower.includes('verify')) {
    return 'Update failed a security check and was rejected';
  }
  if (lower.includes('architecture') || lower.includes('not supported')) {
    return "This build isn't compatible with your Mac";
  }
  if (
    lower.includes('network') ||
    lower.includes('fetch') ||
    lower.includes('dns') ||
    lower.includes('timeout') ||
    lower.includes('connection')
  ) {
    return "Couldn't reach the update server — check your connection";
  }
  return 'Update check failed — see logs for details';
}
