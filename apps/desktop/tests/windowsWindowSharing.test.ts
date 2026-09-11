import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const integrationReady =
  process.platform === 'win32' &&
  process.env.PETAL_WINDOWS_MEDIA_INTEGRATION === '1' &&
  Boolean(process.env.PETAL_WINDOWS_CAPTURE_HWND) &&
  Boolean(process.env.LIVEKIT_URL) &&
  Boolean(process.env.LIVEKIT_API_KEY) &&
  Boolean(process.env.LIVEKIT_API_SECRET);

test('Windows media gate covers the required live recovery matrix', () => {
  const source = readFileSync(
    fileURLToPath(new URL('../src-tauri/src/session_stub.rs', import.meta.url)),
    'utf8'
  );
  const functionStart = source.indexOf(
    'async fn windows_share_session_runs_real_wgc_livekit_audio_lifecycle()'
  );
  assert.ok(functionStart >= 0, 'missing executable Windows media gate');
  const functionEnd = source.indexOf('\n    #[test]', functionStart);
  assert.ok(functionEnd > functionStart, 'Windows media gate must end before the next Rust test');
  const gate = source.slice(functionStart, functionEnd);
  for (const marker of [
    'set_video_quality(VideoQuality::Low)',
    'set_video_quality(VideoQuality::High)',
    'let capable_observer = RoomConnection::connect',
    'publish_window_at(',
    'old_published\n            .unpublish()',
    'capable_replacement_frames.load(Ordering::Acquire) > 0',
    'reconnect the capable LiveKit observer',
    'reconnected_frame',
    'request_stop_for_test()',
    'PETAL_TEST_UNPUBLISH_DELAY_MS',
    'screen_audio_handle_present(window_audio_source)',
    'ScreenAudioCapture::start',
    'system_audio.stop().expect("repeated audio stop")',
    'stop_share_token(&app.handle(), &state, token)'
  ]) {
    assert.ok(gate.includes(marker), `missing live gate marker: ${marker}`);
  }

  // Keep this contract tied to the executable gate's order, not merely to a
  // collection of strings somewhere in the Rust source. The runtime test is
  // opt-in, so this is the always-on guard that prevents a future edit from
  // silently dropping one of the required live phases.
  const phases = [
    'set_video_quality(VideoQuality::Low)',
    'set_video_quality(VideoQuality::High)',
    'start_share_token(',
    'let replacement = Arc::new(',
    'old_published\n            .unpublish()',
    'capable_replacement_frames.load(Ordering::Acquire) > 0',
    'let reconnected = RoomConnection::connect',
    'reconnected_frame',
    'stop_share_token(&app.handle(), &state, token)',
    'request_stop_for_test()',
    'PETAL_TEST_UNPUBLISH_DELAY_MS',
    'ScreenAudioCapture::start',
  ];
  let previous = -1;
  for (const phase of phases) {
    const next = gate.indexOf(phase, previous + 1);
    assert.ok(next > previous, `live gate phase is missing or out of order: ${phase}`);
    previous = next;
  }
});

test(
  'Windows media integration executes the production WGC/LiveKit/audio gate',
  { skip: !integrationReady },
  () => {
    const crateDir = fileURLToPath(new URL('../src-tauri/', import.meta.url));
    const result = spawnSync(
      process.platform === 'win32' ? 'cargo.exe' : 'cargo',
      [
        'test',
        '--lib',
        'session::tests::windows_share_session_runs_real_wgc_livekit_audio_lifecycle',
        '--',
        '--exact',
        '--ignored',
        '--nocapture',
      ],
      {
        cwd: crateDir,
        env: process.env,
        stdio: 'inherit',
        encoding: 'utf8',
      }
    );

    assert.equal(result.error, undefined, result.error?.message);
    assert.equal(result.status, 0, 'the production Windows media gate failed');
  }
);
