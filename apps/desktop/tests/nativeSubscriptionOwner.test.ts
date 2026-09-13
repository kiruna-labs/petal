import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const nativeOwner = readFileSync(
  new URL('../src-tauri/src/transport/native_subscription.rs', import.meta.url),
  'utf8'
);
const publisher = readFileSync(
  new URL('../src-tauri/src/transport/publisher.rs', import.meta.url),
  'utf8'
);
const subscriber = readFileSync(
  new URL('../src-tauri/src/transport/subscriber.rs', import.meta.url),
  'utf8'
);

// The Rust admission-table test proves examples. These source contracts pin
// the architectural wiring around it without manufacturing LiveKit objects.
test('the native room disables auto-subscribe and applies one coordinator', () => {
  assert.match(publisher, /room_options\.auto_subscribe\s*=\s*false;/);
  const connect = publisher.slice(
    publisher.indexOf('pub async fn connect(url: &str, token: &str)'),
    publisher.indexOf('pub fn room(&self)', publisher.indexOf('pub async fn connect(url: &str, token: &str)'))
  );
  const coordinator = connect.indexOf('NativeSubscriptionCoordinator::new');
  const snapshot = connect.indexOf('coordinator.apply_snapshot()');
  const fanout = connect.indexOf('with_connect_event_source_and_coordinator');
  assert.ok(coordinator >= 0 && snapshot > coordinator && fanout > snapshot, connect);
});

test('ownership is decided before either connect-time consumer sees an event', () => {
  const fanout = publisher.slice(
    publisher.indexOf('async fn fanout_connect_events('),
    publisher.indexOf('impl<R> RoomConnection<R>', publisher.indexOf('async fn fanout_connect_events('))
  );
  const observe = fanout.indexOf('coordinator.observe(&event)');
  const compositor = fanout.indexOf('sender.send(event.clone())');
  const resilience = fanout.lastIndexOf('sender.send(event)');
  assert.ok(observe >= 0 && compositor > observe && resilience > observe, fanout);
});

test('camera exclusion lives at admission, not as subscriber compensation', () => {
  assert.match(nativeOwner, /TrackKind::Audio\s*=>\s*true/);
  assert.match(nativeOwner, /TrackKind::Video\s*=>\s*window_id_from_track_name\(name\)\.is_some\(\)/);
  assert.doesNotMatch(subscriber, /set_subscribed\(false\)/);
  assert.match(subscriber, /log_unexpected_native_video/);
});

test('snapshot, event and reconnect all use the same admission function', () => {
  assert.match(nativeOwner, /RoomEvent::TrackPublished[^\n]*\{ publication,[^\n]*\}\s*=>\s*admit\(publication\)/);
  assert.match(nativeOwner, /RoomEvent::Reconnected\s*=>\s*self\.apply_snapshot\(\)/);
  assert.match(nativeOwner, /for participant in self\.room\.remote_participants\(\)\.values\(\)[\s\S]*?admit\(publication\)/);
});
