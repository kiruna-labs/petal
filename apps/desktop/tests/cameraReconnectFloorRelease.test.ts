import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// #275: on Windows every camera publication takes a startup allocation floor
// (`camera_publish_options` -> `startup_allocation_floor`). The reconnect
// republish builds a NEW sender with a new floor, so it must arm its own
// `release_startup_min_bitrate`; the initial publish's task may already have
// exited (it holds at most STARTUP_MIN_BITRATE_MAX_HOLD), leaving the new
// floor pinned for the rest of the call.

const publisher = readFileSync(
  new URL('../src-tauri/src/transport/publisher.rs', import.meta.url),
  'utf8'
);

function methodBody(signature: string): string {
  const start = publisher.indexOf(signature);
  assert.notEqual(start, -1, `could not locate ${signature}`);
  const end = publisher.indexOf('\n    }\n', start);
  assert.notEqual(end, -1, `could not locate the end of ${signature}`);
  return publisher.slice(start, end);
}

test('#275: the camera reconnect republish releases the floor it publishes with', () => {
  const body = methodBody('pub(crate) async fn republish_camera_after_reconnect(');
  const optsAt = body.indexOf('camera_publish_options(');
  const floorAt = body.indexOf('.min_bitrate');
  const publishAt = body.indexOf('.publish_track(');
  const releaseAt = body.indexOf('release_startup_min_bitrate(');
  assert.ok(optsAt !== -1 && floorAt > optsAt, 'the floor must be read from the publish options');
  assert.ok(publishAt > floorAt, 'the floor must be captured before publish_track consumes the options');
  assert.ok(releaseAt > publishAt, 'the release must be armed after a successful republish');
  assert.match(body, /self\.background_cancel\.clone\(\)/);
});
