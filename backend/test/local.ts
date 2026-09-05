// Local integration test for the Petal backend. Exercises the REAL handler code
// paths (slug lockstep, JWT minting/grants, and the RoomServiceClient admin
// path against a live server). Run against a local `livekit-server --dev`:
//
//   LIVEKIT_URL=ws://localhost:7880 LIVEKIT_API_KEY=devkey \
//   LIVEKIT_API_SECRET=secret npm run test:local
//
// Falls back to those local-dev defaults if the env is unset.

import {
  ACCESS_CODE_ALPHABET,
  credentialForAccessCode,
  generateAccessCode,
  livekitRoomName,
  normalizeRoomCredential,
  slugify,
} from '../lib/slug.js';
import {
  handleAdminControl,
  handleCreateRoom,
  handleRoomStatus,
  handleToken,
  HttpError,
} from '../lib/handlers.js';
import { loadLiveKitEnv, roomService } from '../lib/livekit.js';

process.env.LIVEKIT_URL ??= 'ws://localhost:7880';
process.env.LIVEKIT_API_KEY ??= 'devkey';
process.env.LIVEKIT_API_SECRET ??= 'secret';

const ALICE_ID = '11111111-1111-4111-8111-111111111111';
const GALLERY_ID = 'web-22222222-2222-4222-8222-222222222222';

let failures = 0;
function check(name: string, cond: boolean, detail = '') {
  if (cond) {
    console.log(`  ok   ${name}`);
  } else {
    failures++;
    console.error(`  FAIL ${name} ${detail}`);
  }
}

function decodeJwtPayload(jwt: string): any {
  const [, payload] = jwt.split('.');
  return JSON.parse(Buffer.from(payload, 'base64url').toString('utf-8'));
}

async function main() {
  // 1) Slug lockstep — exact values asserted by the native Rust unit tests
  //    (rooms.rs) so all three impls provably agree.
  console.log('slug lockstep (must match native rooms::slugify):');
  check('Design Review -> design-review', slugify('Design Review') === 'design-review');
  check('  design   review   -> design-review', slugify('  design   review  ') === 'design-review');
  check('Design Review! -> design-review', slugify('Design Review!') === 'design-review');
  check('eng-sync -> eng-sync', slugify('eng-sync') === 'eng-sync');
  check('--- -> room', slugify('---') === 'room');
  const credential = credentialForAccessCode('abc-defg-hij')!;
  check('derived room credential is internal-only', credential.startsWith('room-'), credential);
  check('derived room credential has capability suffix', normalizeRoomCredential(credential) === credential, credential);
  check('livekitRoomName requires full credential', livekitRoomName(credential) === `petal-room-${credential}`);
  const generatedCodes = Array.from({ length: 200 }, () => generateAccessCode());
  check('backend generated codes use canonical 3-4-3 shape', generatedCodes.every((code) => /^[a-hjkm-z]{3}-[a-hjkm-z]{4}-[a-hjkm-z]{3}$/.test(code)));
  check('backend generator alphabet matches native/web', ACCESS_CODE_ALPHABET === 'abcdefghjkmnopqrstuvwxyz');

  // 2) Token minting + grants (no server needed — pure crypto).
  console.log('token minting + grants:');
  try {
    await handleToken({ room: 'Eng Sync', identity: ALICE_ID });
    check('bare room names do not mint tokens', false);
  } catch {
    check('bare room names do not mint tokens', true);
  }
  const tok = await handleToken({ room: credential, identity: ALICE_ID, displayName: 'Alice' });
  check('response room uses full credential', tok.room === `petal-room-${credential}`, `got ${tok.room}`);
  check('response carries signaling url', typeof tok.url === 'string' && tok.url.length > 0);
  const claims = decodeJwtPayload(tok.token);
  check('jwt sub = identity', claims.sub === ALICE_ID, `got ${claims.sub}`);
  check('jwt grants roomJoin', claims.video?.roomJoin === true);
  check('jwt grants exact credential room', claims.video?.room === `petal-room-${credential}`, `got ${claims.video?.room}`);
  check('jwt grants canPublishData (telepointers)', claims.video?.canPublishData === true);
  check('jwt grants canUpdateOwnMetadata (native share metadata)', claims.video?.canUpdateOwnMetadata === true);
  check('jwt ttl is meeting-length (~24h)', claims.exp - claims.nbf >= 23 * 60 * 60 && claims.exp - claims.nbf <= 25 * 60 * 60);
  const clampedTok = await handleToken({
    room: credential,
    identity: GALLERY_ID,
    canPublish: false,
    canSubscribe: true,
    canPublishData: false,
    hidden: true,
  });
  const clampedClaims = decodeJwtPayload(clampedTok.token);
  check('public token ignores caller hidden=true', clampedClaims.video?.hidden === false);
  check('public token ignores caller canPublish=false', clampedClaims.video?.canPublish === true);
  check('public token ignores caller canPublishData=false', clampedClaims.video?.canPublishData === true);

  // 3) Live admin path against the running livekit-server (proves the creds
  //    round-trip to a REAL server, i.e. the same secret a participant token is
  //    signed with is accepted by this server).
  console.log('live server admin path (RoomServiceClient):');
  const env = loadLiveKitEnv();
  try {
    const svc = roomService(env);
    const roomName = livekitRoomName(credentialForAccessCode('bbb-cccc-ddd')!);
    await svc.createRoom({ name: roomName, emptyTimeout: 10 });
    const parts = await svc.listParticipants(roomName);
    check('created + listed a real room (0 participants)', Array.isArray(parts) && parts.length === 0);
    await svc.deleteRoom(roomName);
    check('deleted the test room', true);
  } catch (err) {
    failures++;
    console.error(`  FAIL live server admin path — is livekit-server --dev running on :7880? ${(err as Error).message}`);
  }

  // 4) Rooms — LiveKit-backed, no database. create -> visible via the
  //    proof-of-possession status lookup with the human name from metadata.
  console.log('rooms directory (LiveKit-only, create/list, idempotent):');
  try {
    const a = await handleCreateRoom({ name: 'Eng Sync', open: true });
    check('created room has credential slug', normalizeRoomCredential(a.room.slug) === a.room.slug, a.room.slug);
    check('created room livekit name contains credential', a.room.livekitRoom === `petal-room-${a.room.slug}`, a.room.livekitRoom);
    const b = await handleCreateRoom({ name: 'eng-sync' });
    check('same label creates a distinct capability', a.room.livekitRoom !== b.room.livekitRoom);
    const { rooms } = await handleRoomStatus({ rooms: [{ room: a.room.slug }] });
    const found = rooms.find((r) => r.id === a.room.id);
    check('room status is returned for a presented credential', !!found);
    const unproven = await handleRoomStatus({ rooms: [{ room: b.room.slug.replace(/[0-9a-f]$/, 'x') }] });
    check('status omits credentials the caller does not hold', unproven.rooms.length === 0);
    check('human name carried via metadata', found?.name === 'Eng Sync', `got ${found?.name}`);
    check('occupancy is a number', typeof found?.occupancy === 'number');
    check('discovery does not expose credential slug', !('slug' in (found ?? {})));
    check('discovery does not expose LiveKit room name', !('liveKitRoom' in (found ?? {})));
    check('discovery does not expose participant identities', !('participants' in (found ?? {})));
    // cleanup
    await roomService(env).deleteRoom(a.room.livekitRoom);
    await roomService(env).deleteRoom(b.room.livekitRoom);
  } catch (err) {
    failures++;
    console.error(`  FAIL rooms directory — is livekit-server --dev running? ${(err as Error).message}`);
  }

  // 5) Abuse hardening against the live server: a closed room's knock gate and
  //    an admin kick are both enforced by /api/token from real room metadata.
  console.log('closed rooms + sticky kick (live metadata):');
  process.env.PETAL_ADMIN_TOKEN = 'local-admin-token';
  try {
    const closedCode = generateAccessCode();
    const closedCredential = credentialForAccessCode(closedCode)!;
    const closed = await handleCreateRoom({ name: 'Knock Room', open: false, room: closedCredential });
    check('closed room stamped open=false on the live server', closed.room.open === false);
    await handleToken({ room: closedCredential, identity: ALICE_ID }).then(
      () => check('closed room refused a credential-only token request', false),
      (err) => check('closed room refused a credential-only token request', (err as HttpError).status === 403)
    );
    const minted = await handleToken({ room: closedCredential, identity: ALICE_ID, accessCode: closedCode });
    check('closed room minted with the matching access code', minted.room === closed.room.livekitRoom);

    await handleAdminControl(
      { action: 'kick', room: closedCredential, identity: ALICE_ID },
      { authorization: 'Bearer local-admin-token' }
    );
    await handleToken({ room: closedCredential, identity: ALICE_ID, accessCode: closedCode }).then(
      () => check('kicked identity is refused a new token even with the access code', false),
      (err) => check('kicked identity is refused a new token even with the access code', (err as HttpError).status === 403)
    );
    const other = await handleToken({ room: closedCredential, identity: GALLERY_ID, accessCode: closedCode });
    check('a different identity still mints after the kick', other.room === closed.room.livekitRoom);
    const restamped = await handleCreateRoom({ name: 'Knock Room renamed', open: true, room: closedCredential });
    check('native re-stamp preserves the knock gate (#203)', restamped.room.open === false);
    await handleToken({ room: closedCredential, identity: ALICE_ID, accessCode: closedCode }).then(
      () => check('native re-stamp preserves the kick record', false),
      (err) => check('native re-stamp preserves the kick record', (err as HttpError).status === 403)
    );
    await roomService(env).deleteRoom(closed.room.livekitRoom);
  } catch (err) {
    failures++;
    console.error(`  FAIL closed rooms + sticky kick — is livekit-server --dev running? ${(err as Error).message}`);
  }

  console.log('');
  if (failures === 0) {
    console.log('ALL PASSED');
  } else {
    console.error(`${failures} CHECK(S) FAILED`);
    process.exit(1);
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
