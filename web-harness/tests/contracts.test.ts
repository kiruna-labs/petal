// Contract tests: the wire-format invariants that keep this web client
// interoperable with the native Petal app. Run with `npm test`
// (node --test; Node strips the erasable-only TS syntax natively).
//
// If any of these fail, native<->web interop silently breaks -- see each
// section's comment for the native counterpart file.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
  ACCESS_CODE_ALPHABET,
  generateAccessCode,
  generateMeetingCode,
  livekitRoomName,
  meetingDisplayLabelFromCredential,
  normalizeAccessCode,
  normalizeMeetingCode,
  normalizeRoomCredential,
  slugify,
} from '@petal/shared/logic/meetingCode';
import { parseJoinInput } from '@petal/shared/logic/joinInput';
import {
  trackNameForWindow,
  trackNameForCamera,
  cameraWindowId,
  randomWindowId,
  mergeSharedSourceMetadata,
  sharedSourceKindFromMetadata,
  sharedWindowScaleFromMetadata,
  sharedWindowTitleFromMetadata,
  sharedWindowUrlFromMetadata,
  colorProfileFromMetadata,
  sharedWindowZOrderFromMetadata,
  sharedWindowZRankFromMetadata,
  TELEPOINTER_TOPIC,
  REMOTE_CONTROL_TOPIC,
  VIEWER_DEMAND_TOPIC,
  PIPELINE_STATS_TOPIC,
  LATENCY_PROBE_TOPIC,
  DRAW_TOPIC,
  AI_CHAT_TOPIC,
  AI_TRACK_PREFIX,
  aiTrackName,
  isAiTrackName,
  type AiChatMessage,
  type DrawMessage,
  type LatencyProbeMessage,
  type PipelineStatsMessage,
  type RemoteControlMessage,
  type TelepointerMessage,
  type ViewerDemandMessage,
} from '../src/trackNames.ts';
import { decodeRemoteControlHotPath, encodeRemoteControlHotPath, fixedPointCoordinateKey, fnv1a32, parseRemoteControlJson, remoteControlPublishOptions } from '../src/remoteControl.ts';
import { drawPublishOptions, parseDrawPayload } from '../src/draw.ts';
import { aiChatPublishOptions, authorizeAiChatMessage, parseAiChatPayload } from '../src/aiChat.ts';
import { createDrawMessageBuilder } from '../src/drawSender.ts';
import { IDENTITY_COLOR_PALETTE } from '../src/telepointer.ts';
import { MIC_TRACK_NAME } from '../src/controls.ts';

type RemoteControlFixtureMessage = RemoteControlMessage & Record<string, unknown>;
type LatencyProbeFixtureMessage = LatencyProbeMessage & Record<string, unknown>;
type PipelineStatsFixtureMessage = PipelineStatsMessage & Record<string, unknown>;
type DrawFixtureMessage = DrawMessage & Record<string, unknown>;

const contractFixture = JSON.parse(
  readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8'),
) as {
  slugify: Array<{ input: string; slug: string }>;
  roomCredentials: Array<{ input: string; normalized: string; livekitRoom: string }>;
  inviteLinks: Array<{
    label: string;
    accessCode: string;
    credential: string;
    httpsPath: string;
    nativeDeepLink: string;
    webJoinQuery: string;
  }>;
  micTrack: { trackName: string; source: string };
  windowTracks: Array<{ windowId: number; trackName: string }>;
  cameraTracks: Array<{ identity: string; trackName: string }>;
  cameraWindowIds: Array<{ trackName: string; windowId: number }>;
  sourceKindMetadata: {
    metadata: string;
    displayWindowId: number;
    windowWindowId: number;
    missingWindowId: number;
  };
  sourceScaleMetadata: {
    metadata: string;
    downscaledWindowId: number;
    retinaWindowId: number;
    invalidWindowId: number;
    missingWindowId: number;
  };
  windowZOrderMetadata: {
    metadata: string;
    orderedWindowIds: number[];
    frontmostWindowId: number;
    middleWindowId: number;
    backmostWindowId: number;
    missingWindowId: number;
  };
  windowUrlMetadata: {
    metadata: string;
    plainWindowId: number;
    plainUrl: string;
    minimizedWindowId: number;
    minimizedUrl: string;
    nonHttpWindowId: number;
    missingWindowId: number;
  };
  topics: {
    telepointer: string;
    remoteControl: string;
    remoteClipboardText: string;
    viewerDemand: string;
    pipelineStats: string;
    latencyProbe: string;
    draw: string;
    aiChat: string;
  };
  aiTracks: Array<{ windowId: number; trackName: string }>;
  aiChatMessages: Array<{
    name: string;
    reliable: boolean;
    authorizedSenders: 'any-participant' | 'window-owner-only' | 'self-only';
    message: AiChatMessage & Record<string, unknown>;
  }>;
  aiChatEndReasons: string[];
  telepointerFields: string[];
  identityPalette: {
    hash: string;
    names: string[];
    hex: string[];
  };
  viewerDemandFields: string[];
  pipelineStatsMessages: Array<{
    name: string;
    reliable: boolean;
    message: PipelineStatsFixtureMessage;
    fields: string[];
    stageFields: string[];
    captureStateFields: string[];
    captureCpuFields: string[];
    receiverFreezeFields: string[];
  }>;
  drawMessages: Array<{
    name: string;
    reliable: boolean;
    message: DrawFixtureMessage;
    fields: string[];
    pointFields: string[];
  }>;
  latencyProbeMessages: Array<{
    name: string;
    reliable: boolean;
    message: LatencyProbeFixtureMessage;
    fields: string[];
  }>;
  remoteControlMessages: Array<{
    name: string;
    reliable: boolean;
    message: RemoteControlFixtureMessage;
    fields: string[];
  }>;
  remoteClipboardMessages: Array<{
    name: string;
    reliable: boolean;
    message: Record<string, unknown>;
    fields: string[];
  }>;
  remoteClipboardStreams: {
    topic: string;
    mimeType: string;
    directions: string[];
    attributes: string[];
    operationIdHexLength: number;
    maxBytes: number;
    reliability: 'reliable' | 'lossy';
    destination: string;
    successSignals: Record<string, string>;
    textRules: string[];
  };
  remoteControlPacketPolicy: Array<{
    packet: string;
    reliability: 'reliable' | 'lossy';
    destination: 'host' | 'controller' | 'targetParticipant';
    authority: 'authenticatedHost' | 'authenticatedController' | 'authenticatedRemoteControlGrant';
  }>;
  rules: { reliability: { lossy: string[]; reliable: string[] } };
  remoteControlBinaryFrames: Array<{ name: string; hex: string; length: number }>;
  fnv1a32TestVectors: Array<{ input: string; hashHex: string }>;
};

test('remote-control binary fixtures and reliability rule are shared', () => {
  const grantToken = '0123456789abcdef0123456789abcdef';
  const pointer = {
    v: 1 as const, kind: 'pointer' as const, action: 'move' as const, targetUserId: 'native-1', controllerId: 'web-1', windowId: 42, seq: 42,
    grantToken, x: 0.5, y: 0.25, button: -1, buttons: 0, modifiers: { alt: false, ctrl: false, meta: false, shift: false }
  };
  const wheel = {
    v: 1 as const, kind: 'wheel' as const, targetUserId: 'native-1', controllerId: 'web-1', windowId: 42, seq: 43,
    grantToken, x: 0.5, y: 0.25, deltaX: -12, deltaY: 120, deltaMode: 0 as const, modifiers: { alt: false, ctrl: true, meta: false, shift: false }
  };
  assert.deepEqual(contractFixture.rules.reliability.lossy, ['pointer.move.buttons==0', 'wheel']);
  assert.equal(remoteControlPublishOptions(pointer).reliable, false);
  assert.equal(remoteControlPublishOptions(wheel).reliable, false);
  assert.equal(remoteControlPublishOptions({ ...pointer, buttons: 1 }).reliable, true);
  assert.deepEqual(remoteControlPublishOptions(pointer).destinationIdentities, ['native-1']);
  // #370 corrective pass (Bug A): no grant token to fingerprint means no
  // binary frame -- the sender must fall back to JSON instead of sending an
  // unverifiable hot-path packet.
  assert.equal(encodeRemoteControlHotPath({ ...pointer, grantToken: undefined }), null);
  const capableWheel = {
    ...wheel,
    targetKind: 'window' as const,
    shareInstanceId: 'share-42',
    controlSessionId: 'session-42',
    inputId: 'wheel-43',
    inputSeq: 43,
    operationFingerprintVersion: 1 as const,
    operationFingerprint: 'fingerprint-43'
  };
  assert.equal(remoteControlPublishOptions(capableWheel).reliable, true);
  assert.equal(encodeRemoteControlHotPath(capableWheel), null);
  assert.equal(encodeRemoteControlHotPath({ ...wheel, targetKind: 'window' }), null);
  for (const [name, message] of [['pointer-move', pointer], ['wheel', wheel]] as const) {
    const bytes = encodeRemoteControlHotPath(message)!;
    const fixture = contractFixture.remoteControlBinaryFrames.find((candidate) => candidate.name === name)!;
    assert.equal(bytes.length, fixture.length);
    assert.equal(Buffer.from(bytes).toString('hex'), fixture.hex);
    const decoded = decodeRemoteControlHotPath(bytes, message.targetUserId, message.controllerId)!;
    assert.equal(decoded.windowId, message.windowId);
    assert.equal(decoded.seq, message.seq);
    assert.equal(decoded.kind, message.kind);
    assert.ok(Math.abs(decoded.x - message.x) < 1 / 0xffff);
    assert.ok(Math.abs(decoded.y - message.y) < 1 / 0xffff);
  }
  assert.equal(fixedPointCoordinateKey(0.5, 0.25), fixedPointCoordinateKey(0.5 + 1 / 0xffff / 3, 0.25));
});

test('fnv1a32 matches pinned test vectors shared with the Rust implementation', () => {
  const encoder = new TextEncoder();
  for (const vector of contractFixture.fnv1a32TestVectors) {
    assert.equal(fnv1a32(encoder.encode(vector.input)).toString(16).padStart(8, '0'), vector.hashHex, vector.input);
  }
  assert.equal(fnv1a32(new Uint8Array()).toString(16).padStart(8, '0'), '811c9dc5');
  assert.equal(fnv1a32(encoder.encode('a')).toString(16).padStart(8, '0'), 'e40c292c');
});

test('unknown JSON enum values are ignored and the next packet remains parseable', () => {
  assert.equal(parseRemoteControlJson('{"v":1,"kind":"future-kind"}'), null);
  assert.equal(parseRemoteControlJson('{"v":1,"kind":"status","status":"future-status"}'), null);
  assert.equal(parseRemoteControlJson('{"v":1,"kind":"pointer","action":"future-action"}'), null);
  assert.equal(parseRemoteControlJson('{"v":1,"kind":"wheel"}')?.kind, 'wheel');
  const additive = parseRemoteControlJson(
    '{"v":1,"kind":"request","targetUserId":"native","controllerId":"web","windowId":1,"seq":1,"targetKind":"future-target","controllerCapabilities":["legacyControl","future-capability"],"reason":"future-reason"}'
  );
  assert.equal(additive?.kind, 'request');
  assert.equal(additive?.targetKind, undefined);
  assert.deepEqual(additive?.controllerCapabilities, ['legacyControl']);
  assert.equal(additive?.reason, undefined);
});

// ---------------------------------------------------------------------------
// slugify / livekitRoomName MUST stay in lockstep with
// apps/desktop/src-tauri/src/rooms.rs::slugify / livekit_room_name_for
// (commit 24c05a7). The vectors live in contracts/petal-contracts.json and
// are read by both the Rust and web-harness suites.
// ---------------------------------------------------------------------------
test('slugify and room names match the shared native/web fixture', () => {
  for (const vector of contractFixture.slugify) {
    assert.equal(slugify(vector.input), vector.slug, vector.input);
  }
});

test('slugify collapses non-ascii-alphanumeric runs like the native char loop', () => {
  // rooms.rs iterates chars: is_ascii_alphanumeric() keeps, everything else
  // (including non-ASCII letters) collapses to a single '-'. The regex here
  // must behave identically.
  assert.equal(slugify('café crème'), 'caf-cr-me');
  assert.equal(slugify('Eng / Sync #2'), 'eng-sync-2');
  assert.equal(slugify('UPPER lower 123'), 'upper-lower-123');
});

test('room credentials carry unguessable capability material', () => {
  const [vector] = contractFixture.roomCredentials;
  assert.equal(normalizeRoomCredential(vector.input), vector.normalized);
  assert.equal(meetingDisplayLabelFromCredential(vector.normalized), null);
  assert.equal(meetingDisplayLabelFromCredential('eng-sync'), null);
  assert.equal(normalizeRoomCredential('eng-sync'), null);
  assert.equal(livekitRoomName(vector.normalized), vector.livekitRoom);
  assert.throws(() => livekitRoomName(normalizeMeetingCode('  Eng-Sync ')));
  const generated = generateMeetingCode();
  assert.ok(normalizeRoomCredential(generated), generated);
  assert.match(generated, /^room-[0-9a-f]{32}$/);
});

test('access-code alphabet and generation exclude visually ambiguous i/l', () => {
  assert.equal(ACCESS_CODE_ALPHABET.includes('i'), false);
  assert.equal(ACCESS_CODE_ALPHABET.includes('l'), false);
  // Normalization is intentionally broader than generation (backend
  // ACCESS_CODE_RE is /^[a-z]{3}-[a-z]{4}-[a-z]{3}$/) so previously-issued
  // codes containing i/l still parse — matching rooms.rs / backend/lib/slug.ts.
  assert.equal(normalizeAccessCode('abc-defg-hij'), 'abc-defg-hij');
  assert.equal(normalizeAccessCode('abc-defg-hlj'), 'abc-defg-hlj');
  assert.equal(normalizeAccessCode('myq-xfkw-azrp'), null);

  const generated = Array.from({ length: 200 }, () => generateAccessCode());
  assert.ok(generated.every((code) => /^[a-hjkm-z]{3}-[a-hjkm-z]{4}-[a-hjkm-z]{3}$/.test(code)));
  assert.ok(generated.every((code) => !/[il]/.test(code)));
});

test('invite-link vectors parse to the credential and ignore cosmetic labels', () => {
  for (const vector of contractFixture.inviteLinks) {
    assert.deepEqual(
      parseJoinInput(`https://petal.example${vector.httpsPath}`),
      { ok: true, code: vector.credential },
      vector.httpsPath,
    );
    assert.deepEqual(
      parseJoinInput(vector.nativeDeepLink),
      { ok: true, code: vector.credential },
      vector.nativeDeepLink,
    );
    assert.deepEqual(
      parseJoinInput(`https://web.example/${vector.webJoinQuery}`),
      { ok: true, code: vector.credential },
      vector.webJoinQuery,
    );
  }
});

// ---------------------------------------------------------------------------
// Track naming MUST match apps/desktop/src-tauri/src/transport/publisher.rs
// (track_name_for_window, CAMERA_TRACK_PREFIX) and compositor.rs's inverse
// window_id_from_track_name.
// ---------------------------------------------------------------------------
test('window track names match the shared native/web fixture', () => {
  for (const vector of contractFixture.windowTracks) {
    assert.equal(trackNameForWindow(vector.windowId), vector.trackName);
    assert.ok(vector.trackName.startsWith('petal-window-'));
    assert.equal(Number(vector.trackName.slice('petal-window-'.length)), vector.windowId);
  }
});

// #787: both sides hard-coded the literal `'petal-mic'` independently, with no
// test pinning them together, while `transport/audio.rs`'s own comment claimed
// it was documented in CONTRACTS.md (it was not). Native counterpart:
// `transport::audio::MIC_TRACK_NAME` +
// `mic_track_name_matches_the_shared_native_web_fixture`.
test('mic track name matches the shared native/web fixture', () => {
  assert.equal(MIC_TRACK_NAME, contractFixture.micTrack.trackName);
  assert.equal(MIC_TRACK_NAME, 'petal-mic');
  assert.equal(contractFixture.micTrack.source, 'microphone');
});

test('camera track names match the shared native/web fixture', () => {
  for (const vector of contractFixture.cameraTracks) {
    assert.equal(trackNameForCamera(vector.identity), vector.trackName);
    assert.ok(vector.trackName.startsWith('petal-camera-'));
  }
});

test('camera synthetic window ids match the shared native/web fixture', () => {
  for (const vector of contractFixture.cameraWindowIds) {
    assert.equal(cameraWindowId(vector.trackName), vector.windowId);
    assert.ok((vector.windowId & 0x8000_0000) !== 0);
  }
});

test('randomWindowId stays a positive u32 with the high bit clear', () => {
  // High bit is reserved on the native side for camera-derived synthetic
  // window ids (fnv1a(name) | 0x8000_0000) -- see trackNames.ts.
  for (let i = 0; i < 1000; i++) {
    const id = randomWindowId();
    assert.ok(Number.isInteger(id));
    assert.ok(id >= 1);
    assert.ok(id <= 0x7fffffff);
  }
});

test('shared source kind metadata is additive and defaults to window', () => {
  assert.equal(sharedSourceKindFromMetadata('', 42), 'window');
  const metadata = mergeSharedSourceMetadata('{"other":true}', 42, 'display');
  assert.equal(sharedSourceKindFromMetadata(metadata, 42), 'display');
  assert.equal(sharedSourceKindFromMetadata(metadata, 7), 'window');
  assert.deepEqual(JSON.parse(metadata).other, true);
  assert.equal(sharedSourceKindFromMetadata(mergeSharedSourceMetadata(metadata, 42, null), 42), 'window');
});

test('a browser screen share advertises itself as NOT remote-controllable', () => {
  // A browser cannot inject OS input. The scale entry above exists so RC-N2W's
  // emulated host can be asked for control, and it also flips the NATIVE
  // receiver's Control button on -- which, for a real getDisplayMedia share,
  // offers a button whose request can only ever time out. Publishing an
  // explicit denial is what lets the receiver hide it.
  const denied = mergeSharedSourceMetadata(undefined, 7, 'display', { remoteControllable: false });
  assert.equal(JSON.parse(denied).petalWindowRemoteControl['7'], false);
  // Still a well-formed share otherwise.
  assert.equal(JSON.parse(denied).petalWindowScales['7'], 1);

  // Default (the cockpit test-pattern path) stays controllable and must not
  // emit the key at all -- absence means allowed on the Rust side.
  const allowed = mergeSharedSourceMetadata(undefined, 7, 'window');
  assert.equal(JSON.parse(allowed).petalWindowRemoteControl, undefined);

  // Teardown clears it, so a denial cannot outlive its share and suppress the
  // button on the next share that reuses the id.
  const cleared = mergeSharedSourceMetadata(denied, 7, null);
  assert.equal(JSON.parse(cleared).petalWindowRemoteControl, undefined);
});

test('a shared source also advertises a positive window scale (#819)', () => {
  // The native receiver's `remote_control_available` requires a positive
  // petalWindowScales entry for the shared window; a share without one can
  // never be remote-controlled, which made RC-N2W refuse every run.
  const metadata = mergeSharedSourceMetadata(undefined, 7, 'window');
  assert.equal(JSON.parse(metadata).petalWindowScales['7'], 1);
  const cleared = mergeSharedSourceMetadata(metadata, 7, null);
  assert.equal(JSON.parse(cleared).petalWindowScales['7'], undefined);
});

test('shared source kind metadata matches the shared native/web fixture', () => {
  const vector = contractFixture.sourceKindMetadata;
  assert.equal(sharedSourceKindFromMetadata(vector.metadata, vector.displayWindowId), 'display');
  assert.equal(sharedSourceKindFromMetadata(vector.metadata, vector.windowWindowId), 'window');
  assert.equal(sharedSourceKindFromMetadata(vector.metadata, vector.missingWindowId), 'window');
});

test('shared window scale metadata preserves downscaled capture scales', () => {
  const vector = contractFixture.sourceScaleMetadata;
  assert.equal(sharedWindowScaleFromMetadata(vector.metadata, vector.downscaledWindowId), 0.64);
  assert.equal(sharedWindowScaleFromMetadata(vector.metadata, vector.retinaWindowId), 1.5);
  assert.equal(sharedWindowScaleFromMetadata(vector.metadata, vector.invalidWindowId), null);
  assert.equal(sharedWindowScaleFromMetadata(vector.metadata, vector.missingWindowId), null);
  assert.equal(sharedWindowScaleFromMetadata('{not-json', vector.downscaledWindowId), null);
});

test('window z-order metadata matches the shared native/web fixture', () => {
  const vector = contractFixture.windowZOrderMetadata;
  assert.deepEqual(sharedWindowZOrderFromMetadata(vector.metadata), vector.orderedWindowIds);
  assert.equal(sharedWindowZRankFromMetadata(vector.metadata, vector.frontmostWindowId), 0);
  assert.equal(sharedWindowZRankFromMetadata(vector.metadata, vector.middleWindowId), 1);
  assert.equal(sharedWindowZRankFromMetadata(vector.metadata, vector.backmostWindowId), 2);
  assert.equal(sharedWindowZRankFromMetadata(vector.metadata, vector.missingWindowId), null);
});

test('window z-order metadata returns null for an absent or malformed key, not an empty array', () => {
  // Absent key entirely (older sharer) -- must be null, not [].
  assert.equal(sharedWindowZOrderFromMetadata(''), null);
  assert.equal(sharedWindowZOrderFromMetadata(JSON.stringify({ petalWindowKinds: { '42': 'window' } })), null);
  assert.equal(sharedWindowZRankFromMetadata('{}', 42), null);
  assert.equal(sharedWindowZOrderFromMetadata('{not-json'), null);

  // Malformed values.
  assert.equal(sharedWindowZOrderFromMetadata(JSON.stringify({ petalWindowZOrder: 'not-an-array' })), null);
  assert.equal(sharedWindowZOrderFromMetadata(JSON.stringify({ petalWindowZOrder: [42, -1, 99] })), null);
  assert.equal(sharedWindowZOrderFromMetadata(JSON.stringify({ petalWindowZOrder: [42, 'seven', 99] })), null);
  assert.equal(sharedWindowZOrderFromMetadata(JSON.stringify({ petalWindowZOrder: [42, 1.5] })), null);

  // An explicitly-published empty order is valid and distinct from absent.
  assert.deepEqual(sharedWindowZOrderFromMetadata(JSON.stringify({ petalWindowZOrder: [] })), []);
});

test('shared window title and URL metadata match native reader behavior', () => {
  const metadata = JSON.stringify({
    petalWindowTitles: {
      '42': '  SPEC.md - Petal  ',
      '7': '   ',
    },
    petalWindowUrls: {
      '42': ' https://example.com/docs?token=secret#section ',
      '43': 'file:///tmp/nope',
    },
  });

  assert.equal(sharedWindowTitleFromMetadata(metadata, 42), 'SPEC.md - Petal');
  assert.equal(sharedWindowTitleFromMetadata(metadata, 7), null);
  assert.equal(sharedWindowTitleFromMetadata(metadata, 99), null);
  assert.equal(sharedWindowUrlFromMetadata(metadata, 42), 'https://example.com/docs');
  assert.equal(sharedWindowUrlFromMetadata(metadata, 43), null);
  assert.equal(sharedWindowUrlFromMetadata('{not-json', 42), null);
});

// Contract pin (#915): read the SAME fixture entry as the native Rust test
// (transport/publisher.rs's window_url_metadata_matches_shared_contract_fixture)
// so a minimization-rule drift between native and web fails on both sides.
test('shared window URL metadata matches the pinned contract fixture', () => {
  const fixture = contractFixture.windowUrlMetadata;
  assert.equal(
    sharedWindowUrlFromMetadata(fixture.metadata, fixture.plainWindowId),
    fixture.plainUrl,
  );
  assert.equal(
    sharedWindowUrlFromMetadata(fixture.metadata, fixture.minimizedWindowId),
    fixture.minimizedUrl,
  );
  assert.equal(sharedWindowUrlFromMetadata(fixture.metadata, fixture.nonHttpWindowId), null);
  assert.equal(sharedWindowUrlFromMetadata(fixture.metadata, fixture.missingWindowId), null);
});

test('window color profile metadata parses by stringified window id', () => {
  const metadata = JSON.stringify({
    petalWindowColorProfiles: {
      '42': { primaries: 'bt709', transfer: 'srgb', matrix: 'bt709', range: 'full' },
      '7': { primaries: 'bt601-ntsc', transfer: 'bt709', matrix: 'bt601', range: 'video' },
    },
  });

  assert.deepEqual(colorProfileFromMetadata(metadata, 42), { range: 'full' });
  assert.deepEqual(colorProfileFromMetadata(metadata, 7), { range: 'video' });
  assert.equal(colorProfileFromMetadata(metadata, 99), null);
});

test('window color profile metadata returns null for missing or malformed profiles', () => {
  assert.equal(colorProfileFromMetadata('', 42), null);
  assert.equal(colorProfileFromMetadata('{not-json', 42), null);
  assert.equal(colorProfileFromMetadata(JSON.stringify({ petalWindowColorProfiles: { '42': 'full' } }), 42), null);
  assert.equal(
    colorProfileFromMetadata(JSON.stringify({ petalWindowColorProfiles: { '42': { range: 'narrow' } } }), 42),
    null,
  );
});

test('identity palette is pinned for native and web draw ink', () => {
  assert.equal(contractFixture.identityPalette.hash, 'utf16-hash-times-31-mod-6');
  assert.deepEqual(contractFixture.identityPalette.names, ['plum', 'blue', 'green', 'amber', 'lilac', 'slate']);
  assert.deepEqual(contractFixture.identityPalette.hex, [...IDENTITY_COLOR_PALETTE]);
});

// ---------------------------------------------------------------------------
// Telepointer wire format MUST match apps/desktop/src-tauri/src/telepointer.rs
// (topic + exact field names, SPEC.md §4.5).
// ---------------------------------------------------------------------------
test('telepointer topic and JSON field names match telepointer.rs', () => {
  assert.equal(TELEPOINTER_TOPIC, contractFixture.topics.telepointer);
  const msg: TelepointerMessage = { windowId: 42, userId: 'web-1', x: 0.5, y: 0.25, visible: true, activity: 'click', surfaceOwnerId: 'peter2' };
  const parsed = JSON.parse(JSON.stringify(msg));
  assert.deepEqual(Object.keys(parsed).sort(), contractFixture.telepointerFields);
});

// ---------------------------------------------------------------------------
// Viewer-demand wire format MUST match apps/desktop/src-tauri/src/viewer_demand.rs
// (topic + exact field names). Receivers publish this while a remote
// compositor window is open/visible so sharers can keep watched shares Full.
// ---------------------------------------------------------------------------
test('viewer-demand topic and JSON field names are pinned', () => {
  assert.equal(VIEWER_DEMAND_TOPIC, contractFixture.topics.viewerDemand);
  const msg: ViewerDemandMessage = {
    v: 2,
    kind: 'heartbeat',
    targetUserId: 'native-1',
    viewerId: 'web-1',
    windowId: 42,
    seq: 9,
    visible: true,
    width: 1280,
    height: 720,
    scale: 2,
    pixelWidth: 2560,
    pixelHeight: 1440,
    needsRepublish: false,
  };
  const parsed = JSON.parse(JSON.stringify(msg));
  assert.deepEqual(Object.keys(parsed).sort(), contractFixture.viewerDemandFields);
});

// ---------------------------------------------------------------------------
// Pipeline-stats wire format MUST match apps/desktop/src-tauri/src/
// pipeline_stats.rs. It carries only locally measured stages; the authenticated
// data-channel sender is the trusted reporter.
// ---------------------------------------------------------------------------
test('pipeline-stats topic and sender/receiver JSON field names are pinned', () => {
  assert.equal(PIPELINE_STATS_TOPIC, contractFixture.topics.pipelineStats);
  assert.deepEqual(
    contractFixture.pipelineStatsMessages.map((vector) => vector.name),
    ['sender', 'receiver'],
  );

  for (const vector of contractFixture.pipelineStatsMessages) {
    assert.equal(vector.reliable, true, vector.name);
    const parsed = JSON.parse(JSON.stringify(vector.message));
    assert.deepEqual(Object.keys(parsed).sort(), vector.fields, vector.name);
    for (const stage of [parsed.grabbed, parsed.encodedSent, parsed.received, parsed.decoded]) {
      if (stage === null) continue;
      assert.deepEqual(Object.keys(stage).sort(), vector.stageFields, vector.name);
    }
    if (parsed.captureState !== null) {
      assert.deepEqual(Object.keys(parsed.captureState).sort(), vector.captureStateFields, vector.name);
      assert.deepEqual(Object.keys(parsed.captureState.cpu).sort(), vector.captureCpuFields, vector.name);
    }
    if (parsed.receiverFreeze !== null) {
      assert.deepEqual(Object.keys(parsed.receiverFreeze).sort(), vector.receiverFreezeFields, vector.name);
    }
  }
});

// ---------------------------------------------------------------------------
// Draw wire format pins the annotation contract: reliable, batched normalized
// points scoped to the target shared-window owner. Drawer identity/color is
// derived from the authenticated LiveKit sender by receivers.
// ---------------------------------------------------------------------------
test('draw topic and all-variant JSON field names are pinned', () => {
  assert.equal(DRAW_TOPIC, contractFixture.topics.draw);

  assert.deepEqual(
    contractFixture.drawMessages.map((vector) => vector.name),
    ['begin', 'points', 'end', 'text', 'clear', 'camera-begin'],
  );

  for (const vector of contractFixture.drawMessages) {
    const parsed = JSON.parse(JSON.stringify(vector.message));
    assert.deepEqual(Object.keys(parsed).sort(), vector.fields, vector.name);
    for (const point of parsed.points) {
      assert.deepEqual(Object.keys(point).sort(), vector.pointFields, vector.name);
    }
    assert.deepEqual(drawPublishOptions(), { topic: DRAW_TOPIC, reliable: vector.reliable }, vector.name);
    assert.deepEqual(parseDrawPayload(JSON.stringify(vector.message)), vector.message);
  }
});

test('draw camera fixture uses the synthetic high-bit camera window id', () => {
  const cameraVector = contractFixture.cameraWindowIds.find((vector) => vector.trackName === 'petal-camera-web-tester');
  const drawVector = contractFixture.drawMessages.find((vector) => vector.name === 'camera-begin');

  assert.ok(cameraVector);
  assert.ok(drawVector);
  assert.equal(drawVector.message.windowId, cameraVector.windowId);
  assert.ok((drawVector.message.windowId & 0x8000_0000) !== 0);
  assert.deepEqual(parseDrawPayload(JSON.stringify(drawVector.message)), drawVector.message);
});

test('draw parser rejects blank owners, non-normalized points, and multiline text', () => {
  const begin = contractFixture.drawMessages[0].message;
  assert.equal(parseDrawPayload(JSON.stringify({ ...begin, ownerIdentity: ' ' })), null);
  assert.equal(
    parseDrawPayload(JSON.stringify({ ...begin, points: [{ x: 1.1, y: 0.5 }] })),
    null,
  );
  const text = contractFixture.drawMessages.find((vector) => vector.name === 'text')?.message;
  assert.ok(text);
  assert.equal(parseDrawPayload(JSON.stringify({ ...text, text: 'one\ntwo' })), null);
});

test('draw sender emits fixture-compatible begin points and end messages', () => {
  const beginVector = contractFixture.drawMessages[0].message;
  const pointsVector = contractFixture.drawMessages[1].message;
  const endVector = contractFixture.drawMessages[2].message;
  const builder = createDrawMessageBuilder({ createStrokeId: () => beginVector.strokeId ?? '' });
  const target = {
    ownerIdentity: beginVector.ownerIdentity,
    windowId: beginVector.windowId,
  };

  const stroke = builder.begin(target, beginVector.points[0]);
  const emitted = [
    stroke.message,
    ...builder.points(stroke, pointsVector.points),
    builder.end(stroke, null),
  ];

  assert.deepEqual(emitted, [beginVector, pointsVector, endVector]);
  for (const message of emitted) {
    assert.deepEqual(parseDrawPayload(JSON.stringify(message)), message);
    assert.deepEqual(drawPublishOptions(), { topic: DRAW_TOPIC, reliable: true });
  }
});

// ---------------------------------------------------------------------------
// AI chat wire format MUST match apps/desktop/src-tauri/src/ai_chat/wire.rs:
// the topic, the `petal-ai-*` track namespace, every v1 message shape, and --
// most importantly -- the per-kind authorization matrix, which is the whole
// security boundary of this topic. Both suites read the same fixture.
// ---------------------------------------------------------------------------
test('ai-chat topic, track namespace and message payloads are pinned', () => {
  assert.equal(AI_CHAT_TOPIC, contractFixture.topics.aiChat);

  for (const vector of contractFixture.aiTracks) {
    assert.equal(aiTrackName(vector.windowId), vector.trackName);
    assert.ok(vector.trackName.startsWith(AI_TRACK_PREFIX));
    assert.ok(isAiTrackName(vector.trackName));
  }
  // The namespaces must not overlap: a `petal-ai-*` track that classified as a
  // window or camera would surface as a bogus tile or a phantom share.
  assert.equal(isAiTrackName(trackNameForWindow(42)), false);
  assert.equal(isAiTrackName(trackNameForCamera('web-tester')), false);

  assert.deepEqual(
    contractFixture.aiChatMessages.map((vector) => vector.name),
    ['startRequest', 'state', 'stateDisabled', 'pttStart', 'transcript', 'sendText'],
  );

  for (const vector of contractFixture.aiChatMessages) {
    // Reliable, always: session state and transcript lines must not be dropped.
    assert.equal(vector.reliable, true, vector.name);
    assert.deepEqual(aiChatPublishOptions(), { topic: AI_CHAT_TOPIC, reliable: true }, vector.name);
    assert.deepEqual(parseAiChatPayload(JSON.stringify(vector.message)), vector.message, vector.name);
  }
});

test('ai-chat authorization matches the senders the fixture records', () => {
  for (const vector of contractFixture.aiChatMessages) {
    const message = parseAiChatPayload(JSON.stringify(vector.message))!;
    const owner = message.ownerIdentity;
    switch (vector.authorizedSenders) {
      case 'any-participant':
        assert.equal(authorizeAiChatMessage(message, 'someone-else'), null, vector.name);
        break;
      case 'window-owner-only':
        assert.equal(authorizeAiChatMessage(message, owner), null, vector.name);
        assert.equal(authorizeAiChatMessage(message, 'someone-else'), 'notWindowOwner', vector.name);
        break;
      case 'self-only':
        assert.equal(authorizeAiChatMessage(message, 'someone-else'), null, vector.name);
        assert.equal(authorizeAiChatMessage(message, ''), 'notSelf', vector.name);
        break;
      default:
        assert.fail(`unknown authorizedSenders '${vector.authorizedSenders}' in '${vector.name}'`);
    }
    // Wrong version is rejected before any per-kind rule runs.
    assert.equal(
      authorizeAiChatMessage({ ...message, v: 2 } as unknown as AiChatMessage, owner),
      'unsupportedVersion',
      vector.name,
    );
  }
});

test('the ai-chat error vocabulary is a closed pinned set', () => {
  assert.deepEqual(contractFixture.aiChatEndReasons, [
    'stopped',
    'time-limit',
    'disabled',
    'not-shared',
    'busy',
    'rate-limited',
    'hosted-unavailable',
    'offline',
    'mint-failed',
    'model-unavailable',
    'quota',
    'error',
  ]);
  const disabled = contractFixture.aiChatMessages.find((vector) => vector.name === 'stateDisabled')!;
  const parsed = parseAiChatPayload(JSON.stringify(disabled.message))!;
  assert.equal(parsed.type === 'state' && parsed.error, 'disabled');
});

// ---------------------------------------------------------------------------
// Latency-probe wire format MUST match apps/desktop/src-tauri/src/latency_probe.rs
// (topic + ping/pong field names). This is data-channel RTT only, not
// glass-to-glass video latency.
// ---------------------------------------------------------------------------
test('latency-probe topic and ping/pong JSON field names are pinned', () => {
  assert.equal(LATENCY_PROBE_TOPIC, contractFixture.topics.latencyProbe);
  assert.deepEqual(
    contractFixture.latencyProbeMessages.map((vector) => vector.name),
    ['ping', 'pong'],
  );

  for (const vector of contractFixture.latencyProbeMessages) {
    assert.equal(vector.reliable, true, vector.name);
    const parsed = JSON.parse(JSON.stringify(vector.message));
    assert.deepEqual(Object.keys(parsed).sort(), vector.fields, vector.name);
  }
});

// ---------------------------------------------------------------------------
// Remote-control wire format MUST match the native remote-control receiver:
// topic + exact field names for every v1 variant.
// ---------------------------------------------------------------------------
test('remote-control topic and all-variant JSON field names are pinned', () => {
  assert.equal(REMOTE_CONTROL_TOPIC, contractFixture.topics.remoteControl);

  assert.deepEqual(
    contractFixture.remoteControlMessages.map((vector) => vector.name),
    [
      'request',
      'release',
      'status',
      'status-request-unavailable',
      'pointer-move',
      'pointer-down',
      'pointer-up',
      'pointer-click',
      'pointer-double-click',
      'wheel',
      'key',
      'text',
      'pointer-click-v2-canonical-fingerprint',
      'result-applied-v2',
      'result-replay-failed-v2',
      'request-capable-window',
      'status-active-capable-window',
      'pointer-click-capable-window',
      'result-submitted-capable-display',
      'status-controller-upgrade-required',
      'status-awaiting-consent',
      'status-denied',
    ],
  );

  for (const vector of contractFixture.remoteControlMessages) {
    const parsed = JSON.parse(JSON.stringify(vector.message));
    assert.deepEqual(Object.keys(parsed).sort(), vector.fields, vector.name);
    if ('modifiers' in parsed) {
      assert.deepEqual(Object.keys(parsed.modifiers).sort(), ['alt', 'ctrl', 'meta', 'shift'], vector.name);
    }
    assert.deepEqual(
      remoteControlPublishOptions(vector.message),
      {
        topic: REMOTE_CONTROL_TOPIC,
        reliable: vector.reliable,
        // #370 corrective pass (Bug B): every fixture message carries a real
        // targetUserId, so publish options must always scope delivery to it.
        ...(vector.message.targetUserId ? { destinationIdentities: [vector.message.targetUserId] } : {}),
      },
      vector.name,
    );
  }
});

test('native clipboard contract is explicit and unknown copy remains ignored by web peers', () => {
  assert.equal(contractFixture.topics.remoteClipboardText, 'petal.remote-control.clipboard-text');
  assert.deepEqual(contractFixture.remoteClipboardMessages.map((vector) => vector.name), [
    'copy-request',
    'copy-request-capable-window',
  ]);
  for (const copy of contractFixture.remoteClipboardMessages) {
    assert.deepEqual(Object.keys(copy.message).sort(), copy.fields, copy.name);
    assert.equal(copy.message.kind, 'copy');
    assert.equal(typeof copy.message.operationId, 'string');
    assert.equal(parseRemoteControlJson(JSON.stringify(copy.message)), null);
  }
  assert.equal(
    contractFixture.remoteClipboardMessages[1]?.message.shareInstanceId,
    'share-instance-example',
  );

  assert.deepEqual(contractFixture.remoteClipboardStreams, {
    topic: 'petal.remote-control.clipboard-text',
    mimeType: 'text/plain; charset=utf-8',
    directions: ['copyResponse', 'paste'],
    attributes: ['direction', 'grantToken', 'operationId', 'windowId'],
    operationIdHexLength: 32,
    maxBytes: 1_048_576,
    reliability: 'reliable',
    destination: 'oneAuthenticatedParticipant',
    successSignals: { copyResponse: 'targetedTextStreamOnly', paste: 'none' },
    textRules: [
      'nonempty',
      'validUtf8',
      'noNul',
      'plainTextOnly',
      'rejectRecognizedFileClipboardFormats',
      'rejectOversize',
    ],
  });
});

test('remote-control packet policy pins reliability, destination, and authority', () => {
  assert.deepEqual(contractFixture.remoteControlPacketPolicy, [
    { packet: 'request', reliability: 'reliable', destination: 'host', authority: 'authenticatedController' },
    { packet: 'release', reliability: 'reliable', destination: 'host', authority: 'authenticatedController' },
    { packet: 'status', reliability: 'reliable', destination: 'controller', authority: 'authenticatedHost' },
    { packet: 'result', reliability: 'reliable', destination: 'controller', authority: 'authenticatedHost' },
    { packet: 'pointerMoveNoButtons', reliability: 'lossy', destination: 'host', authority: 'authenticatedController' },
    { packet: 'pointerHeldOrDiscrete', reliability: 'reliable', destination: 'host', authority: 'authenticatedController' },
    { packet: 'wheelLegacy', reliability: 'lossy', destination: 'host', authority: 'authenticatedController' },
    { packet: 'scrollDiscrete', reliability: 'reliable', destination: 'host', authority: 'authenticatedController' },
    { packet: 'key', reliability: 'reliable', destination: 'host', authority: 'authenticatedController' },
    { packet: 'text', reliability: 'reliable', destination: 'host', authority: 'authenticatedController' },
    { packet: 'copyRequest', reliability: 'reliable', destination: 'host', authority: 'authenticatedController' },
    { packet: 'clipboardTextStream', reliability: 'reliable', destination: 'targetParticipant', authority: 'authenticatedRemoteControlGrant' },
  ]);
});

test('remote-control fixture pins representative variant payloads', () => {
  const byName = new Map(contractFixture.remoteControlMessages.map((vector) => [vector.name, vector.message]));

  assert.equal(byName.get('request')?.kind, 'request');
  assert.equal(byName.get('release')?.kind, 'release');
  assert.equal(byName.get('result-applied-v2')?.kind, 'result');
  assert.equal(byName.get('pointer-click-v2-canonical-fingerprint')?.operationFingerprintVersion, 1);
  assert.equal(byName.get('pointer-click-capable-window')?.shareInstanceId, 'share-instance-example');
  assert.equal(byName.get('status-active-capable-window')?.targetKind, 'window');
  assert.equal(byName.get('result-submitted-capable-display')?.outcome, 'submitted');
  assert.equal(byName.get('status-controller-upgrade-required')?.reason, 'controllerUpgradeRequired');
  // Consent flow (ask policy): the parked-request status carries no grant
  // and no reason; the deny carries the consent reason.
  assert.equal(byName.get('status-awaiting-consent')?.status, 'awaitingConsent');
  assert.equal(byName.get('status-awaiting-consent')?.grantToken, undefined);
  assert.equal(byName.get('status-denied')?.status, 'denied');
  assert.equal(byName.get('status-denied')?.reason, 'consentDenied');
  const submitted = parseRemoteControlJson(
    JSON.stringify(byName.get('result-submitted-capable-display')),
  );
  assert.equal(submitted?.kind, 'result');
  if (submitted?.kind === 'result') assert.equal(submitted.outcome, 'submitted');
  assert.deepEqual(byName.get('status'), {
    v: 1,
    targetUserId: 'web-1',
    controllerId: 'native-1',
    windowId: 42,
    seq: 14,
    grantToken: '0123456789abcdef0123456789abcdef',
    kind: 'status',
    status: 'active',
    message: 'Remote control active for shared window',
    supportsBinaryHotPath: true,
  });
  assert.equal(byName.get('status-request-unavailable')?.status, 'requestUnavailable');
  assert.deepEqual(byName.get('pointer-move'), {
    v: 1,
    targetUserId: 'native-1',
    controllerId: 'web-1',
    windowId: 42,
    seq: 8,
    grantToken: '0123456789abcdef0123456789abcdef',
    kind: 'pointer',
    action: 'move',
    x: 0.5,
    y: 0.25,
    button: -1,
    buttons: 0,
    modifiers: { alt: false, ctrl: false, meta: false, shift: true },
  });
  assert.deepEqual(byName.get('wheel'), {
    v: 1,
    targetUserId: 'native-1',
    controllerId: 'web-1',
    windowId: 42,
    seq: 11,
    grantToken: '0123456789abcdef0123456789abcdef',
    kind: 'wheel',
    x: 0.25,
    y: 0.75,
    deltaX: -12,
    deltaY: 120,
    deltaMode: 0,
    modifiers: { alt: false, ctrl: true, meta: false, shift: false },
  });
  assert.deepEqual(byName.get('pointer-click'), {
    v: 1,
    targetUserId: 'native-1',
    controllerId: 'web-1',
    windowId: 42,
    seq: 15,
    grantToken: '0123456789abcdef0123456789abcdef',
    kind: 'pointer',
    action: 'click',
    x: 0.5,
    y: 0.25,
    button: 0,
    buttons: 0,
    modifiers: { alt: false, ctrl: false, meta: false, shift: true },
  });
  assert.deepEqual(byName.get('pointer-double-click'), {
    v: 1,
    targetUserId: 'native-1',
    controllerId: 'web-1',
    windowId: 42,
    seq: 16,
    grantToken: '0123456789abcdef0123456789abcdef',
    kind: 'pointer',
    action: 'down',
    x: 0.5,
    y: 0.25,
    button: 0,
    buttons: 1,
    clickCount: 2,
    modifiers: { alt: false, ctrl: false, meta: false, shift: false },
  });
  assert.deepEqual(byName.get('key'), {
    v: 1,
    targetUserId: 'native-1',
    controllerId: 'web-1',
    windowId: 42,
    seq: 12,
    grantToken: '0123456789abcdef0123456789abcdef',
    kind: 'key',
    action: 'down',
    key: 'c',
    code: 'KeyC',
    repeat: false,
    location: 0,
    modifiers: { alt: false, ctrl: false, meta: true, shift: false },
  });
  assert.deepEqual(byName.get('text'), {
    v: 1,
    targetUserId: 'native-1',
    controllerId: 'web-1',
    windowId: 42,
    seq: 13,
    grantToken: '0123456789abcdef0123456789abcdef',
    kind: 'text',
    text: 'hello',
    modifiers: { alt: false, ctrl: false, meta: false, shift: true },
  });
});
