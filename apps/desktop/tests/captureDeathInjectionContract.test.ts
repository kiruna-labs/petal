import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const capture = readFileSync(new URL('../src-tauri/src/windows_screen_capture.rs', import.meta.url), 'utf8');
const session = readFileSync(new URL('../src-tauri/src/session_stub.rs', import.meta.url), 'utf8');

// Source-wiring checks only, not a substitute for live deadline acceptance.
test('debug capture death also stops cached frame delivery before the pump select', () => {
  assert.match(capture, /state\.test_delivery_stopped\.store\(true, Ordering::Release\);\s*signal\.request_stop\(\);/);
  assert.match(session, /#\[cfg\(debug_assertions\)\]\s*status\.clone\(\),/);
  assert.match(session, /#\[cfg\(debug_assertions\)\]\s*capture_status: crate::windows_screen_capture::CaptureStatus/);
  const start = session.indexOf('fn start_share_frame_pump(');
  const select = session.indexOf('tokio::select!', start);
  const guard = session.slice(start, select);
  assert.match(guard, /#\[cfg\(debug_assertions\)\]\s*if capture_status\.test_delivery_stopped\(\) \{[\s\S]*?break;/);
});

test('normal static refresh remains present; receiver FPS policy is not the injector', () => {
  assert.match(session, /const SHARE_IDLE_REFRESH_INTERVAL:[^;]*from_secs\(2\);/);
  assert.match(session, /idle-refresh pushed last frame \(static content\)/);
  assert.match(capture, /#\[cfg\(debug_assertions\)\]\s*fn schedule_test_capture_stop/);
});
