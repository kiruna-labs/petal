import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const lib = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');
const sessionStub = readFileSync(
  new URL('../src-tauri/src/session_stub.rs', import.meta.url),
  'utf8'
);
const meetingCore = readFileSync(
  new URL('../src-tauri/src/meeting_core.rs', import.meta.url),
  'utf8'
);
const macRoomSession = readFileSync(
  new URL('../src-tauri/src/session/room.rs', import.meta.url),
  'utf8'
);
const buildScript = readFileSync(new URL('../src-tauri/build.rs', import.meta.url), 'utf8');
const targetRegistry = readFileSync(
  new URL('../src-tauri/src/windows_capture_target.rs', import.meta.url),
  'utf8'
);

test('Windows bootstrap manages the durable room store', () => {
  assert.match(
    lib,
    /Windows rooms persistence loading[\s\S]*app\.manage\(rooms::RoomsState::load\(app_data_dir\)\)/
  );
});

test('Windows loads the same room-token environment as macOS before bootstrapping Tauri', () => {
  const windowsRun = lib.slice(lib.indexOf('pub fn run() {', lib.indexOf('#[cfg(not(target_os = "macos"))]')));
  const desktopEnv = windowsRun.indexOf(
    'load_env_file(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"))'
  );
  const legacyEnv = windowsRun.indexOf(
    'load_env_file(concat!(env!("CARGO_MANIFEST_DIR"), "/.env"))'
  );
  const builder = windowsRun.indexOf('tauri::Builder::default()');

  assert.ok(desktopEnv >= 0, 'Windows must load apps/desktop/.env');
  assert.ok(legacyEnv > desktopEnv, 'legacy src-tauri/.env must remain the fallback');
  assert.ok(builder > legacyEnv, 'token configuration must load before Tauri accepts join commands');
  assert.doesNotMatch(lib, /#\[cfg\(target_os = "macos"\)\]\s*fn load_env_file/);
  // An absent PETAL_BACKEND_URL must bake NOTHING. This repository is public,
  // so a hosted fallback would mean every third-party build silently minting
  // tokens against the maintainers' LiveKit/Vercel deployment.
  assert.match(
    buildScript,
    /Err\(_\) => None/,
    'an unconfigured build must bake no token-backend URL'
  );
  assert.doesNotMatch(
    buildScript,
    /petal\.live/,
    'build.rs must not carry a hosted token-backend default'
  );
});

test('Windows bootstrap exposes the portable room membership contract', () => {
  assert.match(lib, /\.manage\(session::SessionState::default\(\)\)/);
  assert.match(
    lib,
    /session::join_room_command,\s*session::leave_room_command,\s*session::current_room,\s*session::room_presence,\s*session::remote_control_allowed,\s*session::set_remote_control_allowed,/
  );
  assert.match(sessionStub, /pub async fn join_room_command\(/);
  assert.match(sessionStub, /crate::meeting_core::connect_room\(/);
  assert.match(sessionStub, /pub async fn leave_room_command\(/);
  assert.match(sessionStub, /pub fn current_room\(state:/);
  assert.match(sessionStub, /pub fn room_presence\(/);
});

test('portable membership core does not depend on capture or compositor permissions', () => {
  assert.match(meetingCore, /pub\(crate\) async fn connect_room\(/);
  assert.match(meetingCore, /fetch_access_token/);
  assert.match(meetingCore, /RoomConnection::connect/);
  assert.match(macRoomSession, /crate::meeting_core::persist_joined_room_record/);
  assert.match(macRoomSession, /crate::meeting_core::connect_room/);
  assert.doesNotMatch(
    meetingCore,
    /crate::window_source::|crate::permissions::|crate::compositor::|crate::capture::/
  );
});

test('Windows HWND values stay behind an opaque process-local token registry', () => {
  assert.match(lib, /#\[cfg\(target_os = "windows"\)\]\s*mod windows_capture_target;/);
  assert.match(targetRegistry, /raw_handle:\s*usize/);
  assert.match(targetRegistry, /by_token:\s*HashMap<u32,\s*WindowsCaptureTarget>/);
  assert.match(targetRegistry, /UnknownOrStale\(u32\)/);
  assert.doesNotMatch(targetRegistry, /raw_handle\s+as\s+u32/);
});

test('Windows camera devices and publication use native session ownership', () => {
  // All camera commands now come from the cfg-free shared `camera_session`
  // module on both platforms.
  assert.match(
    lib,
    /camera_session::list_camera_devices,\s*camera_session::list_camera_modes,\s*camera_session::set_camera_device,\s*camera_session::set_camera_prefs,/
  );
  assert.match(
    lib,
    /camera_session::start_camera_publish_command,\s*camera_session::stop_camera_publish_command,\s*camera_session::camera_publish_state,/
  );
  assert.doesNotMatch(
    lib,
    /unsupported_media::start_camera_publish_command,\s*unsupported_media::stop_camera_publish_command,\s*unsupported_media::camera_publish_state,/
  );
  // Native capture stays platform-owned: the MF adapter opens the device, and
  // the shared session layer publishes + pumps frames.
  assert.match(sessionStub, /camera:\s*Option<ActiveCamera>/);
  const cameraSession = readFileSync(
    new URL('../src-tauri/src/camera_session.rs', import.meta.url),
    'utf8'
  );
  const mfAdapter = readFileSync(
    new URL('../src-tauri/src/transport/camera/mf.rs', import.meta.url),
    'utf8'
  );
  assert.match(mfAdapter, /start_with_device/);
  assert.match(cameraSession, /\.publish_camera\(/);
  assert.match(cameraSession, /\.push_nv12\(/);
  assert.match(cameraSession, /open_camera\(/);
});
