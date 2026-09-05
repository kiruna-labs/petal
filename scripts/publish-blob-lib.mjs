// Pure, dependency-free logic for scripts/publish-blob.mjs (issue #671),
// split out so it can be unit-tested (scripts/test-publish-blob-lib.mjs)
// without real Vercel Blob credentials or a built app bundle. Nothing in
// this file touches the network or the filesystem.

// Compares two "MAJOR.MINOR.PATCH" version strings. Returns negative if
// a < b, zero if equal, positive if a > b. Throws on a malformed version
// string rather than silently treating it as comparable -- a publish gate
// that can't parse a version must fail closed, not fail open.
export function compareVersions(a, b) {
  const parse = (v) => {
    const parts = String(v).trim().split('.');
    if (parts.length !== 3 || parts.some((p) => !/^\d+$/.test(p))) {
      throw new Error(`compareVersions: not a MAJOR.MINOR.PATCH version: ${JSON.stringify(v)}`);
    }
    return parts.map(Number);
  };
  const [aMaj, aMin, aPatch] = parse(a);
  const [bMaj, bMin, bPatch] = parse(b);
  if (aMaj !== bMaj) return aMaj - bMaj;
  if (aMin !== bMin) return aMin - bMin;
  return aPatch - bPatch;
}

// True if `liveVersion` is already >= `newVersion` -- i.e. publishing
// `newVersion` would be a downgrade, OR a no-op republish that overwrites
// the live manifest's tarball/signature pairing with different bytes at the
// same version number. Both are refused: see #671 item 5.
export function isDowngradeOrSame(liveVersion, newVersion) {
  return compareVersions(liveVersion, newVersion) >= 0;
}

// Stable blob pathname for the versioned updater tarball. Mirrors the DMG's
// existing pattern (`Petal_<version>_universal.dmg`) so a release's updater
// artifact is never overwritten by the next one -- rollback is just
// re-publishing the previous manifest, whose tarball still exists at its
// own versioned path (#671 item 2).
export function updaterTarballPathname(version) {
  return `Petal_${version}_universal.app.tar.gz`;
}

// Stable pathname for the Windows x86-64 NSIS installer. The build filename
// comes from Tauri, but the Blob pathname is our distribution contract and
// must remain platform-specific for download routing and rollback.
export function windowsInstallerPathname(version) {
  return `Petal_${version}_windows_x86_64-setup.exe`;
}

// Build the one manifest published after every platform artifact has uploaded.
// Keeping this pure makes it difficult for a platform publisher to accidentally
// replace the other platform entries or publish a partial latest.json.
export function buildUpdaterManifest({ version, notes, pubDate, darwinUrl, darwinSignature, windowsUrl, windowsSignature }) {
  const platforms = {
    'darwin-aarch64': { signature: darwinSignature, url: darwinUrl },
    'darwin-x86_64': { signature: darwinSignature, url: darwinUrl },
  };
  // A macOS-only publish (Windows lane paused, user directive 2026-08-25)
  // omits the windows entry entirely: Windows builds then report up-to-date
  // via the updater's TargetsNotFound path instead of hitting a stale URL.
  if (windowsUrl && windowsSignature) {
    platforms['windows-x86_64'] = { signature: windowsSignature, url: windowsUrl };
  }
  return {
    version,
    notes,
    pub_date: pubDate,
    platforms,
  };
}

// Release notes (#671 item 7): prefer the annotated git tag's own message;
// fall back to the previous hardcoded "Petal <tag>" format only when the tag
// has no annotation (a lightweight tag, or nothing readable).
export function resolveReleaseNotes(tagAnnotation, tag) {
  const trimmed = (tagAnnotation ?? '').trim();
  return trimmed.length > 0 ? trimmed : `Petal ${tag}`;
}

// Staple gate (plan item 1, 2026-08-10). A notarized `.app` carries its
// stapled ticket as a file at `Contents/CodeResources` (magic `s8ch`), so a
// tarball created BEFORE `xcrun stapler staple` ran simply has no such
// member, and one created before a *re-staple* has the wrong bytes. Presence
// alone is therefore not sufficient evidence -- a stale ticket from a
// previous version would still be present and still be wrong -- which is why
// the caller compares hashes against the ticket on the app it just validated.
// Pure so it can be unit-tested without a notarized build; the I/O half lives
// in publish-blob.mjs's verifyStapledInsideTarball().
export function assertUpdaterTarballIsStapled({ tarballName, diskTicketSha256, tarTicketSha256 }) {
  if (!diskTicketSha256) {
    throw new Error(
      `staple gate: refusing to publish ${tarballName}; no on-disk notarization ticket was read ` +
        `from the built Petal.app, so there is nothing to compare the tarball against. ` +
        `Notarize + staple the .app first (docs/RELEASING.md step 2).`
    );
  }
  if (!tarTicketSha256) {
    throw new Error(
      `staple gate: refusing to publish ${tarballName}; it carries NO notarization ticket ` +
        `(Petal.app/Contents/CodeResources is absent), so it was created before the .app was ` +
        `stapled. Auto-update users would install a build that fails an offline Gatekeeper ` +
        `check. Re-create the tarball from the stapled .app -- docs/RELEASING.md step 2b.`
    );
  }
  if (tarTicketSha256 !== diskTicketSha256) {
    throw new Error(
      `staple gate: refusing to publish ${tarballName}; its notarization ticket ` +
        `(sha256 ${tarTicketSha256}) is not the one stapled to the .app on disk ` +
        `(sha256 ${diskTicketSha256}). The tarball predates the current staple. Re-create it ` +
        `from the stapled .app -- docs/RELEASING.md step 2b.`
    );
  }
}

// Per-slice baked-value matcher (#874). A universal binary's per-arch slices
// are independently compiled, so a value baked into one slice says nothing
// about the other -- `strings` on the FAT binary can't tell "baked in both"
// from "baked in arm64 only, x86_64 built without the env var." This is the
// per-slice primitive: given ONE slice's raw bytes (already `lipo -thin`'d
// out by the caller) and the expected string value, decide whether that
// slice genuinely carries it.
//
// Two ways a slice can carry a value:
//   - contiguous: the literal bytes appear as one unbroken run (a normal
//     .rodata string constant).
//   - chunked: LLVM sometimes materializes a string literal as a sequence of
//     inline `movabs $imm64` immediates instead of a contiguous byte run
//     (confirmed on x86_64 -- see #874's issue body for the `lipo -thin` +
//     chunk-count repro). Split the value into 8-byte chunks (the width of a
//     single movabs immediate; the last chunk may be shorter) and require
//     EVERY chunk to be present, in order: chunk 1's first occurrence at
//     some position p1, chunk 2's first occurrence at some position p2 > p1,
//     and so on. Chunks all present but only out of order (e.g. chunk 2
//     occurs only before every chunk-1 occurrence) must NOT count as baked
//     -- that's coincidental byte overlap, not a real compiled-in value.
//
// Pure and dependency-free (no filesystem, no `lipo`/`strings` subprocess)
// so it's unit-testable against synthetic buffers without a real build --
// the I/O half (thinning the binary, running `strings`) lives in
// publish-blob.mjs's callers.
export function valueIsBakedInSlice(sliceBuffer, value) {
  const needle = Buffer.isBuffer(value) ? value : Buffer.from(value, 'utf8');
  const haystack = Buffer.isBuffer(sliceBuffer) ? sliceBuffer : Buffer.from(sliceBuffer);

  if (needle.length === 0) {
    throw new Error('valueIsBakedInSlice: expected value must not be empty');
  }

  if (haystack.includes(needle)) {
    return { baked: true, mode: 'contiguous', detail: 'contiguous' };
  }

  const CHUNK_SIZE = 8;
  const chunks = [];
  for (let i = 0; i < needle.length; i += CHUNK_SIZE) {
    chunks.push(needle.subarray(i, i + CHUNK_SIZE));
  }

  // Per-chunk total occurrence count, for the diagnostic detail string
  // (e.g. "chunked 29/2/2") regardless of whether the ordered chain
  // ultimately succeeds -- useful evidence either way.
  const counts = chunks.map((chunk) => {
    let count = 0;
    let from = 0;
    for (;;) {
      const idx = haystack.indexOf(chunk, from);
      if (idx === -1) break;
      count += 1;
      from = idx + 1;
    }
    return count;
  });

  if (counts.some((c) => c === 0)) {
    return { baked: false, mode: null, detail: `miss (chunk counts: ${counts.join('/')})` };
  }

  // Greedy ordered chain: chunk 1's first occurrence, then chunk 2's first
  // occurrence strictly after that position, and so on. If this chain
  // completes, an in-order chunked bake exists. Greedy (always taking the
  // earliest available occurrence) is sufficient here: taking the earliest
  // possible position for chunk k never forecloses a later chunk k+1's
  // occurrence that a later choice for chunk k would also have allowed.
  const chainCompletes = (ordered) => {
    let searchFrom = 0;
    for (const chunk of ordered) {
      const idx = haystack.indexOf(chunk, searchFrom);
      if (idx === -1) return false;
      searchFrom = idx + 1;
    }
    return true;
  };

  if (chainCompletes(chunks)) {
    return { baked: true, mode: 'chunked', detail: `chunked ${counts.join('/')}` };
  }

  // LLVM emits the movabs+store sequence in REVERSE (last chunk at the
  // lowest address, ~14 bytes apart descending) -- the layout measured on
  // the real 0.9.3 x86_64 PostHog bake, and the one the #874 evidence in
  // docs/RELEASING.md describes. A descending file layout is an ascending
  // chain over the reversed chunk list.
  if (chainCompletes([...chunks].reverse())) {
    return { baked: true, mode: 'chunked', detail: `chunked ${counts.join('/')} (descending)` };
  }

  return { baked: false, mode: null, detail: `miss (chunk counts: ${counts.join('/')}, order broken)` };
}
