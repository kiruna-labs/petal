#!/usr/bin/env node
// Unit tests for scripts/publish-blob-lib.mjs -- the pure logic extracted
// from scripts/publish-blob.mjs so the downgrade-refusal, versioned-tarball
// pathname, and release-notes logic (issue #671 items 2/5/7) can be verified
// without real Vercel Blob credentials or a built app bundle.
//
// NOT covered here (would need real credentials / a real build to exercise
// end-to-end -- see the PR/report for what remains manually or CI-verified
// only): the actual list()/fetch()/put() calls in verifyNotDowngrade() and
// upload(), and the AppleDouble/universal/Sentry-DSN gates (those already
// existed and are unchanged by this issue).

import assert from 'node:assert/strict';
import {
  assertUpdaterTarballIsStapled,
  compareVersions,
  isDowngradeOrSame,
  updaterTarballPathname,
  windowsInstallerPathname,
  buildUpdaterManifest,
  resolveReleaseNotes,
  valueIsBakedInSlice,
} from './publish-blob-lib.mjs';

function fail(message) {
  console.error(`FAIL: ${message}`);
  process.exitCode = 1;
}

try {
  // compareVersions
  assert.equal(compareVersions('0.8.3', '0.8.3'), 0, 'equal versions compare equal');
  assert.ok(compareVersions('0.8.3', '0.8.4') < 0, 'patch bump compares smaller');
  assert.ok(compareVersions('0.8.4', '0.8.3') > 0, 'patch bump compares larger the other way');
  assert.ok(compareVersions('0.9.0', '0.8.99') > 0, 'minor beats patch');
  assert.ok(compareVersions('1.0.0', '0.99.99') > 0, 'major beats minor/patch');
  assert.ok(compareVersions('0.8.10', '0.8.9') > 0, 'numeric, not lexicographic, patch comparison');
  assert.throws(
    () => compareVersions('not-a-version', '0.8.3'),
    /not a MAJOR\.MINOR\.PATCH version/,
    'malformed version fails closed, not silently 0'
  );
  assert.throws(() => compareVersions('0.8.3', '0.8'), /not a MAJOR\.MINOR\.PATCH version/, 'incomplete version rejected');
  assert.throws(() => compareVersions('0.8.3', 'v0.8.3'), /not a MAJOR\.MINOR\.PATCH version/, "leading 'v' rejected (caller must strip it)");
  console.log('PASS: compareVersions');

  // isDowngradeOrSame
  assert.equal(isDowngradeOrSame('0.8.3', '0.8.4'), false, 'publishing newer than live is allowed');
  assert.equal(isDowngradeOrSame('0.8.4', '0.8.4'), true, 'republishing the SAME version is refused too');
  assert.equal(isDowngradeOrSame('0.8.5', '0.8.4'), true, 'publishing older than live is refused');
  console.log('PASS: isDowngradeOrSame');

  // updaterTarballPathname
  assert.equal(updaterTarballPathname('0.8.4'), 'Petal_0.8.4_universal.app.tar.gz');
  assert.notEqual(
    updaterTarballPathname('0.8.4'),
    'Petal_universal.app.tar.gz',
    'must not regress to the old unversioned pathname'
  );
  assert.notEqual(
    updaterTarballPathname('0.8.4'),
    updaterTarballPathname('0.8.5'),
    'two different versions must never collide on the same pathname'
  );
  console.log('PASS: updaterTarballPathname');

  // windowsInstallerPathname + combined updater manifest
  assert.equal(
    windowsInstallerPathname('0.8.4'),
    'Petal_0.8.4_windows_x86_64-setup.exe',
    'Windows installer path is versioned and identifies its architecture'
  );
  assert.notEqual(
    windowsInstallerPathname('0.8.4'),
    windowsInstallerPathname('0.8.5'),
    'different Windows releases must not collide'
  );
  const manifest = buildUpdaterManifest({
    version: '0.8.4',
    notes: 'Windows release',
    pubDate: '2026-08-24T00:00:00.000Z',
    darwinUrl: 'https://cdn.example/Petal_0.8.4_universal.app.tar.gz',
    darwinSignature: 'darwin-signature',
    windowsUrl: 'https://cdn.example/Petal_0.8.4_windows_x86_64-setup.exe',
    windowsSignature: 'windows-signature',
  });
  assert.deepEqual(Object.keys(manifest.platforms), [
    'darwin-aarch64',
    'darwin-x86_64',
    'windows-x86_64',
  ], 'combined manifest preserves both Darwin keys and adds Windows');
  assert.equal(manifest.platforms['darwin-aarch64'].url, manifest.platforms['darwin-x86_64'].url);
  assert.equal(manifest.platforms['darwin-aarch64'].signature, 'darwin-signature');
  assert.equal(manifest.platforms['windows-x86_64'].url, 'https://cdn.example/Petal_0.8.4_windows_x86_64-setup.exe');
  assert.equal(manifest.platforms['windows-x86_64'].signature, 'windows-signature');
  const macOnlyManifest = buildUpdaterManifest({
    version: '0.9.3',
    notes: 'macOS-only release',
    pubDate: '2026-08-25T00:00:00.000Z',
    darwinUrl: 'https://cdn.example/Petal_0.9.3_universal.app.tar.gz',
    darwinSignature: 'darwin-signature',
  });
  assert.deepEqual(Object.keys(macOnlyManifest.platforms), [
    'darwin-aarch64',
    'darwin-x86_64',
  ], 'a macOS-only publish omits the windows key rather than emitting a broken entry');
  console.log('PASS: windowsInstallerPathname + buildUpdaterManifest');

  // resolveReleaseNotes
  assert.equal(resolveReleaseNotes('Fixes the thing.\n', 'v0.8.4'), 'Fixes the thing.', 'annotated message wins, trimmed');
  assert.equal(resolveReleaseNotes('', 'v0.8.4'), 'Petal v0.8.4', 'falls back on an empty (lightweight) tag');
  assert.equal(resolveReleaseNotes('   \n', 'v0.8.4'), 'Petal v0.8.4', 'falls back on whitespace-only annotation');
  assert.equal(resolveReleaseNotes(undefined, 'v0.8.4'), 'Petal v0.8.4', 'falls back on undefined');
  assert.equal(resolveReleaseNotes(null, 'v0.8.4'), 'Petal v0.8.4', 'falls back on null');
  console.log('PASS: resolveReleaseNotes');

  // assertUpdaterTarballIsStapled (plan item 1). The local release procedure
  // used to publish the tarball built in step 1, before step 2 stapled the
  // .app -- so auto-update users got a build with no notarization ticket.
  const STAPLED = 'a'.repeat(64);
  const OTHER = 'b'.repeat(64);
  assert.doesNotThrow(
    () => assertUpdaterTarballIsStapled({
      tarballName: 'Petal.app.tar.gz',
      diskTicketSha256: STAPLED,
      tarTicketSha256: STAPLED,
    }),
    'a tarball re-created from the stapled .app carries the same ticket and publishes'
  );
  assert.throws(
    () => assertUpdaterTarballIsStapled({
      tarballName: 'Petal.app.tar.gz',
      diskTicketSha256: STAPLED,
      tarTicketSha256: null,
    }),
    /carries NO notarization ticket/,
    'a tarball built BEFORE stapling has no ticket member and is refused'
  );
  assert.throws(
    () => assertUpdaterTarballIsStapled({
      tarballName: 'Petal.app.tar.gz',
      diskTicketSha256: STAPLED,
      tarTicketSha256: OTHER,
    }),
    /is not the one stapled to the \.app on disk/,
    'a STALE ticket from a previous staple is refused too -- presence alone is not evidence'
  );
  assert.throws(
    () => assertUpdaterTarballIsStapled({
      tarballName: 'Petal.app.tar.gz',
      diskTicketSha256: null,
      tarTicketSha256: STAPLED,
    }),
    /no on-disk notarization ticket was read/,
    'fails closed when there is no on-disk ticket to compare against'
  );
  console.log('PASS: assertUpdaterTarballIsStapled');

  // valueIsBakedInSlice (#874). The per-slice matcher a universal build's
  // Sentry/PostHog/backend-URL publish gates lean on: a value can be baked
  // as a contiguous byte run OR as an in-order chain of 8-byte movabs
  // chunks (the x86_64 immediate-materialization artifact from the issue
  // body's repro: whole=0 but every chunk present == baked).
  {
    const VALUE = 'https://app.petal.live'; // 23 bytes -> chunks: 8 / 8 / 7
    const chunk1 = VALUE.slice(0, 8); // 'https://'
    const chunk2 = VALUE.slice(8, 16); // 'app.peta'
    const chunk3 = VALUE.slice(16); // 'l.live'

    // (a) contiguous hit: the literal value appears as one unbroken run.
    const contiguousBuf = Buffer.from(`noise before ${VALUE} noise after`);
    const contiguousResult = valueIsBakedInSlice(contiguousBuf, VALUE);
    assert.equal(contiguousResult.baked, true, 'contiguous run is baked');
    assert.equal(contiguousResult.mode, 'contiguous', 'contiguous run reports contiguous mode');
    console.log('PASS: valueIsBakedInSlice contiguous hit');

    // (b) chunked hit: chunks scattered in order among noise, never
    // contiguous, mirroring the real movabs artifact (each chunk isolated
    // by unrelated bytes, chunk1 before chunk2 before chunk3).
    const chunkedBuf = Buffer.from(
      `junk1 ${chunk1} filler-aaaa junk2 ${chunk2} filler-bbbb junk3 ${chunk3} tail`
    );
    const chunkedResult = valueIsBakedInSlice(chunkedBuf, VALUE);
    assert.equal(chunkedResult.baked, true, 'in-order scattered chunks are baked');
    assert.equal(chunkedResult.mode, 'chunked', 'scattered chunks report chunked mode');
    assert.match(chunkedResult.detail, /chunked 1\/1\/1/, 'detail reports per-chunk occurrence counts');
    console.log('PASS: valueIsBakedInSlice chunked hit (scattered, in order)');

    // (b2) chunked hit, DESCENDING: LLVM emits the movabs+store sequence in
    // reverse, so the file layout is last-chunk-lowest, ~14 bytes apart
    // (13 for a trailing partial chunk). Measured on the real 0.9.3 x86_64
    // PostHog bake: offsets 9335560/9335546/9335532/.../9335491 -- the gate
    // must accept this or it blocks a genuinely-baked release.
    const descendingBuf = Buffer.alloc(160, 0x90);
    let descOff = 120;
    for (const c of [chunk1, chunk2, chunk3]) {
      descendingBuf.write(c, descOff);
      descOff -= 14;
    }
    const descendingResult = valueIsBakedInSlice(descendingBuf, VALUE);
    assert.equal(descendingResult.baked, true, 'descending movabs layout is baked');
    assert.equal(descendingResult.mode, 'chunked', 'descending layout reports chunked mode');
    assert.match(descendingResult.detail, /descending/, 'detail says the chain matched descending');
    console.log('PASS: valueIsBakedInSlice chunked hit (descending movabs layout)');

    // (c) MISS: all chunks present, but neither an ascending nor a
    // descending chain exists (layout chunk1, chunk3, chunk2 breaks both
    // directions). All-chunks-present must not be conflated with baked.
    const outOfOrderBuf = Buffer.from(
      `${chunk1} some filler ${chunk3} more filler ${chunk2}`
    );
    const outOfOrderResult = valueIsBakedInSlice(outOfOrderBuf, VALUE);
    assert.equal(outOfOrderResult.baked, false, 'chunks present but order-scrambled must NOT count as baked');
    assert.equal(outOfOrderResult.mode, null, 'out-of-order miss reports null mode');
    console.log('PASS: valueIsBakedInSlice miss (chunks present, order broken)');

    // (d) plain miss: a chunk is genuinely absent from the slice.
    const absentBuf = Buffer.from(`${chunk1} filler ${chunk2} filler, no third chunk here`);
    const absentResult = valueIsBakedInSlice(absentBuf, VALUE);
    assert.equal(absentResult.baked, false, 'a genuinely absent chunk is a miss');
    assert.equal(absentResult.mode, null, 'absent-chunk miss reports null mode');
    console.log('PASS: valueIsBakedInSlice miss (chunk absent)');

    // (e) short value (< 8 bytes, single chunk): contiguous check alone
    // must suffice -- e.g. the 'phc_' PostHog prefix class of value.
    const shortValue = 'phc_ab'; // 6 bytes, shorter than one chunk
    const shortBuf = Buffer.from(`noise ${shortValue} noise`);
    const shortResult = valueIsBakedInSlice(shortBuf, shortValue);
    assert.equal(shortResult.baked, true, 'short (<8 byte) value found contiguously is baked');
    assert.equal(shortResult.mode, 'contiguous', 'short value hit reports contiguous mode');
    console.log('PASS: valueIsBakedInSlice short value contiguous hit');

    // Sanity: a short value truly absent is a miss, not a false positive.
    const shortMissResult = valueIsBakedInSlice(Buffer.from('nothing relevant here'), shortValue);
    assert.equal(shortMissResult.baked, false, 'short value genuinely absent is a miss');
    console.log('PASS: valueIsBakedInSlice short value miss');
  }
  console.log('PASS: valueIsBakedInSlice');

  console.log('\nALL publish-blob-lib unit tests passed.');
} catch (err) {
  fail(err.stack || err.message);
}
