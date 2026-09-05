import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const captureSource = readFileSync(
  new URL('../src-tauri/src/windows_screen_capture.rs', import.meta.url),
  'utf8'
);
const platformWindowsSource = readFileSync(
  new URL('../src-tauri/src/platform/windows.rs', import.meta.url),
  'utf8'
);
const shareTargetSource = readFileSync(
  new URL('../src-tauri/src/share_target.rs', import.meta.url),
  'utf8'
);
const windowSourceSource = readFileSync(
  new URL('../src-tauri/src/window_source.rs', import.meta.url),
  'utf8'
);
const overlaySource = readFileSync(
  new URL('../src-tauri/src/windows_share_overlay.rs', import.meta.url),
  'utf8'
);
const regionSource = readFileSync(
  new URL('../src/routes/region-window/+page.svelte', import.meta.url),
  'utf8'
);
const regionNativeSource = readFileSync(
  new URL('../src-tauri/src/region_window.rs', import.meta.url),
  'utf8'
);
const remoteControlSource = readFileSync(
  new URL('../src-tauri/src/remote_control.rs', import.meta.url),
  'utf8'
);
const libSource = readFileSync(
  new URL('../src-tauri/src/lib.rs', import.meta.url),
  'utf8'
);
const ipcSource = readFileSync(new URL('../src/lib/ipc.ts', import.meta.url), 'utf8');
const hoverTabSource = readFileSync(
  new URL('../src/routes/hover-tab/+page.svelte', import.meta.url),
  'utf8'
);
const windowsHoverSource = readFileSync(
  new URL('../src-tauri/src/windows_hover.rs', import.meta.url),
  'utf8'
);
const captureTargetSource = readFileSync(
  new URL('../src-tauri/src/windows_capture_target.rs', import.meta.url),
  'utf8'
);
const pointerSource = readFileSync(
  new URL('../src/routes/compositor/pointer/+page.svelte', import.meta.url),
  'utf8'
);
const sessionSource = readFileSync(
  new URL('../src-tauri/src/session_stub.rs', import.meta.url),
  'utf8'
);
const liveExercise = readFileSync(
  new URL('../scripts/windows-petal-view-placement-smoke.ps1', import.meta.url),
  'utf8'
);
const hoverTabSmoke = readFileSync(
  new URL('../scripts/windows-hover-tab-smoke.ps1', import.meta.url),
  'utf8'
);
const displayExercise = readFileSync(
  new URL('../scripts/windows-display-indicator-smoke.ps1', import.meta.url),
  'utf8'
);
const trackingExercise = readFileSync(
  new URL('../scripts/windows-share-overlay-tracking-smoke.ps1', import.meta.url),
  'utf8'
);
const architecture = readFileSync(
  new URL('../../../docs/ARCHITECTURE.md', import.meta.url),
  'utf8'
);
const audit = readFileSync(
  new URL('../../../docs/WINDOWS_NATIVE_SURFACE_AUDIT.md', import.meta.url),
  'utf8'
);

test('picker and hover share one native eligibility classifier', () => {
  assert.match(shareTargetSource, /pub\(crate\) fn classify\(/);
  assert.match(shareTargetSource, /ShareTargetRejection/);
  assert.match(platformWindowsSource, /pub\(crate\) fn inspect_window\(/);
  assert.match(windowSourceSource, /inspect_window\(hwnd, self_pid\)/);
  assert.match(windowSourceSource, /share_target::classify\(&inspection\.facts\)/);
  assert.match(windowsHoverSource, /inspect_window\(hwnd, std::process::id\(\)\)/);
  assert.match(windowsHoverSource, /share_target::classify\(&inspection\.facts\)/);
  assert.match(platformWindowsSource, /class\.eq_ignore_ascii_case\("ControlCenterWindow"\)/);
  assert.doesNotMatch(windowSourceSource, /fn should_include_window|fn should_include_source_window/);
  assert.doesNotMatch(platformWindowsSource, /fn is_shareable_window/);
});

test('Petal View has a persistent selector control while ordinary hover remains separate', () => {
  assert.match(regionSource, /class="region-share-control"/);
  assert.match(hoverTabSource, /displayLike/);
  assert.match(windowsHoverSource, /HoverTabUpdate/);
});

test('Petal View options use label-addressed native authority', () => {
  assert.match(regionNativeSource, /pub async fn region_view_options_state/);
  assert.match(regionNativeSource, /pub async fn set_region_share_priority/);
  assert.match(regionNativeSource, /pub fn set_region_draw_active/);
  assert.match(regionNativeSource, /pub async fn region_ai_chat_start/);
  assert.match(regionNativeSource, /pub fn region_ai_chat_stop/);
  assert.match(regionNativeSource, /ensure_region_token\(&app, &window_label\)/);
  assert.match(regionSource, /COMMANDS\.regionViewOptionsState/);
  assert.match(regionSource, /COMMANDS\.setRegionSharePriority/);
  assert.match(regionSource, /COMMANDS\.setRegionDrawActive/);
  assert.match(regionSource, /COMMANDS\.regionAiChatStart/);
  assert.match(regionSource, /COMMANDS\.regionAiChatStop/);
  assert.match(libSource, /region_window::region_view_options_state/);
  assert.match(libSource, /region_window::set_region_share_priority/);
  assert.match(ipcSource, /regionViewOptionsState: 'region_view_options_state'/);
  assert.match(ipcSource, /regionViewOptionsChanged: 'region-view-options-changed'/);
  const optionsStruct = regionNativeSource.slice(
    regionNativeSource.indexOf('pub(crate) struct RegionViewOptionsState'),
    regionNativeSource.indexOf('pub(crate) struct RegionViewOptionsChanged')
  );
  assert.doesNotMatch(optionsStruct, /window_id|windowId/);
});

test('Petal View controller state is projected by label without exposing IDs', () => {
  assert.match(regionNativeSource, /RegionControlStateChanged/);
  assert.match(regionNativeSource, /emit_region_control_state_for_status/);
  assert.match(regionNativeSource, /active_controller_display_name/);
  assert.match(remoteControlSource, /emit_region_control_state_for_status\(app, &status\)/);
  assert.match(ipcSource, /regionControlStateChanged: 'region-control-state-changed'/);
  assert.match(regionSource, /EVENTS\.regionControlStateChanged/);
  assert.match(regionSource, /controllerStatus/);
  assert.doesNotMatch(regionSource, /controllerId/);
});

test('Windows ordinary capture uses an explicit indicator policy instead of an unconditional system border', () => {
  assert.match(captureSource, /enum\s+CaptureIndicatorMode/);
  assert.match(captureSource, /CaptureIndicatorMode::System/);
  assert.match(captureSource, /CaptureIndicatorMode::Petal/);
  assert.match(captureSource, /owner_verified/);
  assert.match(captureSource, /CaptureSourceKind::Window\s*=>\s*replacement_ready\s*&&\s*owner_verified/);
  assert.match(captureSource, /RequestAccessAsync\(\s*GraphicsCaptureAccessKind::Borderless/);
  assert.match(captureSource, /static ACCESS:\s*OnceLock/);
  assert.match(
    captureSource,
    /fn\s+capture_indicator_mode[\s\S]{0,1600}replacement_ready/,
    'ordinary window/display capture must decide whether a Petal replacement is ready'
  );
  assert.match(
    captureSource,
    /let mut system_border_required = indicator_mode\.system_border_required\(\)[\s\S]{0,300}SetIsBorderRequired\(system_border_required\)/,
    'WGC border visibility must be driven by the selected indicator mode'
  );
  assert.doesNotMatch(
    captureSource,
    /SetIsBorderRequired\(region\.is_none\(\)\)/,
    'ordinary capture must not be permanently pinned to the WGC system border'
  );
});

test('the existing sharer overlay is the single local Petal indicator and is fail-safe for displays', () => {
  assert.match(overlaySource, /ShareOverlayReadiness/);
  assert.match(overlaySource, /PageLoadEvent::Finished/);
  assert.match(overlaySource, /OVERLAY_PAGE_LOAD_TIMEOUT/);
  assert.match(overlaySource, /custom_indicator_ready/);
  assert.match(overlaySource, /shareBorder/);
  assert.match(overlaySource, /custom_indicator_requested/);
  assert.match(overlaySource, /set_capture_exclusion/);
  assert.match(overlaySource, /SWP_NOACTIVATE/);
  assert.match(overlaySource, /overlay_owner_matches/);
  assert.match(overlaySource, /GW_OWNER/);
  assert.match(overlaySource, /SWP_NOZORDER/);
  assert.match(overlaySource, /map_err\(\|error\|/);
  assert.match(overlaySource, /target\.kind\(\) == TargetKind::Display \|\| region_share/);
  assert.match(pointerSource, /sharer-share-border/);
  assert.match(pointerSource, /border:\s*4px solid var\(--sharer-share-border-color\)/);
  assert.match(pointerSource, /border-radius:\s*var\(--radius-input\)/);
  assert.match(pointerSource, /pointer-events:\s*none/);
  assert.match(pointerSource, /sharerSurface && page\.url\.searchParams\.get\('shareBorder'\)/);
  assert.match(sessionSource, /request_borderless_access\(\)/);
  assert.match(sessionSource, /acquire_selector_capture_exclusion\(&app, token\)/);
  assert.match(sessionSource, /create_share_overlay\(/);
  assert.match(sessionSource, /close_share_overlay\(&app, token\)/);
  const overlayStart = sessionSource.indexOf('create_share_overlay(');
  const exclusionStart = sessionSource.indexOf('acquire_selector_capture_exclusion(&app, token)');
  const captureStart = sessionSource.indexOf('TargetCaptureSession::start(');
  assert.ok(overlayStart >= 0 && captureStart > overlayStart, 'the custom indicator must be prepared before WGC starts');
  assert.ok(exclusionStart >= 0 && captureStart > exclusionStart, 'selector exclusion must be acquired before WGC starts');
});

test('ordinary-window indicators use native ownership and retain safe fallback', () => {
  const createStart = overlaySource.indexOf('pub(crate) fn create_share_overlay(');
  const createEnd = overlaySource.indexOf('\n/// Tear down the sharer overlay', createStart);
  assert.ok(createStart >= 0 && createEnd > createStart, 'sharer overlay creation boundary disappeared');
  const create = overlaySource.slice(createStart, createEnd);
  assert.doesNotMatch(create, /\.always_on_top\(true\)/);
  assert.match(create, /owner_raw\(/, 'same-integrity ordinary overlays use their source as the Win32 owner');
  assert.match(create, /window_integrity_exceeds_petal/);
  assert.match(overlaySource, /OverlayStackingMode/);
  assert.match(overlaySource, /SourceOwned/);
  assert.match(overlaySource, /Passive/);
  assert.match(overlaySource, /window_integrity_exceeds_petal/);
  assert.match(overlaySource, /custom_indicator_requested[\s\S]{0,240}SourceOwned/);
  assert.match(overlaySource, /EVENT_SYSTEM_FOREGROUND/);
  assert.match(overlaySource, /GW_OWNER/);
  assert.match(overlaySource, /overlay_owner_matches/);
  assert.doesNotMatch(overlaySource, /ElevatedBand/);
  assert.doesNotMatch(overlaySource, /HWND_BOTTOM/);
  assert.doesNotMatch(overlaySource, /HWND_TOP(?!MOST)/);
  assert.doesNotMatch(overlaySource, /GW_HWNDPREV/);
  assert.doesNotMatch(overlaySource, /GeometryAndZOrder/);
  assert.doesNotMatch(overlaySource, /ZOrderOnly/);
  assert.doesNotMatch(overlaySource, /source_z_order_anchor|choose_z_order_anchor|ZOrderAnchor/);
  assert.doesNotMatch(overlaySource, /TargetKind::Window\s*=>\s*overlay_is_directly_above/);
  assert.match(overlaySource, /TargetKind::Display[\s\S]{0,700}HWND_TOPMOST/);
  assert.match(overlaySource, /IsWindow/);
  assert.match(overlaySource, /IsWindowVisible/);
  assert.match(overlaySource, /source_owned_overlay_needs_fallback/);
  assert.match(overlaySource, /#\[ignore = "requires an interactive Windows desktop"\]/);
  assert.match(overlaySource, /request_system_indicator_fallback/);
  assert.match(captureSource, /CaptureSignal/);
  assert.match(captureSource, /system_indicator_requested/);
  assert.match(captureSource, /SetIsBorderRequired\(true\)/);
  assert.match(captureSource, /restore_system_indicator_before_disable/);
  assert.match(captureSource, /capture_signal_request_wakes_the_waiting_capture_thread/);
  assert.match(captureSource, /capture indicator fallback|system indicator fallback/i);
  assert.match(captureSource, /CaptureIndicatorMode::System/);
});

test('runtime indicator fallback is one-shot and terminal when WGC cannot restore its border', () => {
  assert.match(captureSource, /request_system_indicator_fallback/);
  assert.match(captureSource, /system_indicator_restored/);
  assert.match(captureSource, /system_indicator_pending/);
  assert.match(captureSource, /unregister_capture_signal/);
  assert.match(captureSource, /terminal.*indicator|indicator.*terminal/i);
  assert.match(overlaySource, /disable_custom_indicator_for_fallback/);
});

test('Petal View capture exclusion is scoped to active capture ownership', () => {
  assert.match(regionNativeSource, /SelectorCaptureExclusionLease/);
  assert.match(regionNativeSource, /window_label/);
  assert.match(regionNativeSource, /release_selector_capture_exclusion/);
  assert.match(sessionSource, /selector_capture_exclusion/);
  assert.match(sessionSource, /drop_share_capture\(capture\)\.await[\s\S]{0,180}release_selector_capture_exclusion\(selector_capture_exclusion\)/);
  assert.match(sessionSource, /release_selector_capture_exclusion\(selector_capture_exclusion\);/);
  assert.match(regionNativeSource, /rebind|reissued/);
  assert.match(regionNativeSource, /newer Share.*lease|stale teardown/);
});

test('failed starts and terminal cleanup restore idle recordability', () => {
  const startStart = sessionSource.indexOf('pub(crate) async fn start_share_token(');
  const startEnd = sessionSource.indexOf('\nfn start_share_url_refresh(', startStart);
  assert.ok(startStart >= 0 && startEnd > startStart, 'Windows share start boundary disappeared');
  const start = sessionSource.slice(startStart, startEnd);
  assert.match(start, /release_selector_capture_exclusion\(selector_capture_exclusion\);/);
  assert.match(start, /drop_share_capture\(capture\)\.await[\s\S]{0,180}release_selector_capture_exclusion\(selector_capture_exclusion\)/);
  const stopStart = sessionSource.indexOf('async fn stop_share(');
  const stopEnd = sessionSource.indexOf('\npub\(crate\) async fn stop_share_token(', stopStart);
  assert.ok(stopStart >= 0 && stopEnd > stopStart, 'Windows share stop boundary disappeared');
  const stop = sessionSource.slice(stopStart, stopEnd);
  const captureDrop = stop.indexOf('drop_share_capture(capture).await');
  const exclusionRelease = stop.indexOf('release_selector_capture_exclusion', captureDrop);
  assert.ok(captureDrop >= 0 && exclusionRelease > captureDrop, 'capture must be dropped before selector inclusion is restored');
  assert.match(sessionSource, /emit_share_state_changed\(app, token, false\)/);
});

test('affinity is per selector, not a global recording switch', () => {
  assert.match(regionNativeSource, /stable selector label|stable Tauri label|window_label/);
  assert.match(regionNativeSource, /stored_hwnd|original_hwnd|hwnd/);
  assert.match(regionNativeSource, /lease_id/);
  assert.match(regionNativeSource, /capture_exclusion_owners/);
  assert.match(sessionSource, /if is_region_share/);
  assert.match(sessionSource, /selector_capture_exclusion:\s*Option/);
  assert.doesNotMatch(regionNativeSource, /static\s+\w*(?:CAPTURE_)?AFFINITY/);
});

test('selector lifecycle contracts cover failure, leave, close, and independent selectors', () => {
  assert.match(sessionSource, /start_share_loss_monitor/);
  assert.match(sessionSource, /stop_share\(&app, share, room_connection\.clone\(\)\)/);
  assert.match(sessionSource, /CaptureFailed|capture failed/);
  assert.match(sessionSource, /close_all_region_windows\(app\)/);
  assert.match(regionNativeSource, /cleanup_region_window_state/);
  assert.match(regionNativeSource, /windows_capture_target::invalidate\(token\)/);
  assert.match(regionNativeSource, /selector_label_from_title/);
});

test('the hover-tab smoke is a gated native red-capable positive-control loop', () => {
  assert.match(hoverTabSmoke, /-PetalPid/);
  assert.match(hoverTabSmoke, /another Petal binary is already running/);
  assert.match(hoverTabSmoke, /ProductName/);
  assert.match(hoverTabSmoke, /StartSacrificialWindow/);
  assert.match(hoverTabSmoke, /FindVisibleTitle\('Hover Tab'\)/);
  assert.match(hoverTabSmoke, /FindVisibleShellSurface/);
  assert.match(hoverTabSmoke, /Wait-ForShellSurface/);
  assert.match(hoverTabSmoke, /tray fallback/);
  assert.match(hoverTabSmoke, /ControlCenterWindow/);
  assert.match(hoverTabSmoke, /ShellHost/);
  assert.match(hoverTabSmoke, /PressChord/);
  assert.match(hoverTabSmoke, /quick-settings/);
  assert.match(hoverTabSmoke, /MoveCursor/);
  assert.match(hoverTabSmoke, /FactAtCursor/);
  assert.match(hoverTabSmoke, /GetClassName/);
  assert.match(hoverTabSmoke, /GetWindowLongPtr/);
  assert.match(hoverTabSmoke, /GetAncestor/);
  assert.match(hoverTabSmoke, /DwmGetWindowAttribute/);
  assert.match(hoverTabSmoke, /SetWindowFrame/);
  assert.match(hoverTabSmoke, /Invoke-FollowPositiveControl/);
  assert.match(hoverTabSmoke, /Invoke-ContinuousFollow/);
  assert.match(hoverTabSmoke, /borderCurrentTabPrevious/);
  assert.match(hoverTabSmoke, /visibilityGap/);
  assert.match(hoverTabSmoke, /8ms/);
  assert.match(hoverTabSmoke, /detectorWentRed/);
  assert.match(hoverTabSmoke, /pickerDecision/);
  assert.match(hoverTabSmoke, /expected .*right-center square/);
  assert.match(hoverTabSmoke, /40x40 tab after dwell/);
  assert.match(hoverTabSmoke, /Right-click opens the native/);
  assert.match(hoverTabSmoke, /RightClickAt/);
  assert.match(hoverTabSmoke, /FindVisibleClass\('#32768'\)/);
  assert.match(hoverTabSmoke, /right-click-native-menu/);
  assert.match(hoverTabSmoke, /native-menu-escape/);
  assert.match(hoverTabSmoke, /direct Share action/);
  assert.match(hoverTabSmoke, /direct Stop action/);
  assert.match(hoverTabSmoke, /Invoke-ShareStopReuse/);
  assert.match(hoverTabSmoke, /Share->Stop->Share->Stop/);
  assert.match(hoverTabSmoke, /ordinary-outside/);
  assert.match(hoverTabSmoke, /maximized-inset/);
  assert.match(hoverTabSmoke, /\$\{Label\}-share-stop-reuse/);
  assert.match(hoverTabSmoke, /Surface = 'none'/);
  assert.match(hoverTabSmoke, /Screen\]::FromHandle/);
  assert.match(hoverTabSmoke, /WorkingArea/);
  assert.match(hoverTabSmoke, /Check-WorkAreaContainment/);
  assert.match(hoverTabSmoke, /Invoke-TaskbarEdgePlacement/);
  assert.match(hoverTabSmoke, /taskbar-edge-placement/);
  assert.match(hoverTabSmoke, /cursor transfer/);
  assert.match(hoverTabSmoke, /direct Share click/);
  assert.match(hoverTabSmoke, /direct Stop click/);
  assert.match(hoverTabSmoke, /ExercisePosition/);
  assert.match(hoverTabSmoke, /ExerciseOcclusion/);
  assert.match(hoverTabSmoke, /StartOccluderWindow/);
  assert.match(hoverTabSmoke, /PlaceAboveNoActivate/);
  assert.match(hoverTabSmoke, /IsAboveInZOrder/);
  assert.match(hoverTabSmoke, /occluderAboveTab/);
  assert.match(hoverTabSmoke, /tabAboveSource/);
  assert.match(hoverTabSmoke, /Invoke-OcclusionExercise/);
  assert.match(hoverTabSmoke, /Invoke-NativeQualityPreset/);
  assert.match(hoverTabSmoke, /Invoke-NativePositionPreset/);
  assert.match(hoverTabSmoke, /hoverTabVerticalOffset/);
  assert.match(hoverTabSmoke, /Select-NativeMenuEntry/);
  assert.match(hoverTabSmoke, /share-priority: saved/);
  assert.match(hoverTabSmoke, /Invoke-ActiveShareMenuActions/);
  assert.match(hoverTabSmoke, /draw request applied/);
  assert.match(hoverTabSmoke, /share control mode changed/);
  assert.doesNotMatch(windowsHoverSource, /dismissal_generation|set_hover_tab_presentation|HOVER_TAB_ESCALATION_HEIGHT/);
  assert.match(windowsHoverSource, /Hide on the Tauri main thread/);
  assert.match(windowsHoverSource, /adopt_hover_target_replacement/);
  assert.match(windowsHoverSource, /cached_hover_target_is_stale/);
  assert.match(windowsHoverSource, /project_hover_tab_native_frame/);
  assert.match(windowsHoverSource, /reconcile_native_hover_tab/);
  assert.match(overlaySource, /native_event_targets_follower/);
  assert.match(overlaySource, /source_frames/);
  assert.match(platformWindowsSource, /window_dpi_scale/);
  assert.match(platformWindowsSource, /monitor_frame_for_window/);
  assert.match(sessionSource, /replace_hover_tab_follower_token/);
  assert.match(captureTargetSource, /retire_for_hover/);
  assert.match(captureTargetSource, /consume_hover_replacement/);
  assert.match(captureTargetSource, /UnknownOrStale/);
  assert.match(sessionSource, /kind == SharedSourceKind::Window[\s\S]{0,320}current_hover_presentation/);
  assert.match(sessionSource, /revoke_window\(app, token, "share stopped"\)[\s\S]{0,500}retire_for_hover\(token\)/);
});

test('Windows hover native placement stays source-relative and queued', () => {
  const createStart = windowsHoverSource.indexOf('pub fn create_pill_window(');
  const createEnd = windowsHoverSource.indexOf('\nfn hide_pill(', createStart);
  assert.ok(createStart >= 0 && createEnd > createStart, 'hover-tab creation boundary disappeared');
  assert.doesNotMatch(windowsHoverSource.slice(createStart, createEnd), /\.always_on_top\(true\)/);

  const placementStart = windowsHoverSource.indexOf('fn apply_native_hover_tab_placement(');
  const placementEnd = windowsHoverSource.indexOf('\n/// Reconcile the native tab', placementStart);
  assert.ok(placementStart >= 0 && placementEnd > placementStart, 'hover-tab placement boundary disappeared');
  const placement = windowsHoverSource.slice(placementStart, placementEnd);
  assert.match(placement, /checked_window_above_in_z_order_excluding/);
  assert.match(placement, /SWP_NOACTIVATE/);
  assert.doesNotMatch(placement, /SWP_NOZORDER/);

  const admissionStart = overlaySource.indexOf('fn native_event_targets_follower(');
  const callbackStart = overlaySource.indexOf('unsafe extern "system" fn overlay_win_event_proc');
  const callbackEnd = overlaySource.indexOf('\nfn install_overlay_hooks', callbackStart);
  assert.ok(admissionStart >= 0 && callbackStart > admissionStart, 'follower event boundary disappeared');
  const admission = overlaySource.slice(admissionStart, callbackStart);
  assert.match(admission, /ignored_hwnd == Some\(hwnd\)[\s\S]{0,80}return false/);
  assert.match(admission, /event == EVENT_OBJECT_REORDER && !reorder_is_top_level[\s\S]{0,80}return false/);
  assert.match(admission, /native_reorder_event_is_top_level\(hwnd\)/);

  assert.ok(callbackStart >= 0 && callbackEnd > callbackStart, 'WinEvent callback boundary disappeared');
  assert.match(overlaySource.slice(callbackStart, callbackEnd), /post_tracker_reconcile\(\)/);
});

test('hover placement uses work-area bounds while the border keeps the full source frame', () => {
  assert.match(platformWindowsSource, /monitor_work_area_for_window/);
  assert.match(platformWindowsSource, /rcWork/);
  assert.match(windowsHoverSource, /monitor_work_area_for_window/);
  assert.match(windowsHoverSource, /project_hover_tab_native_frame/);
  assert.match(overlaySource, /visible_window_frame/);
  assert.match(overlaySource, /source_frames/);
});

test('the bounded Windows placement exercise uses SendInput, screen capture, and a log tail', () => {
  assert.match(liveExercise, /SendInput/);
  assert.match(liveExercise, /CopyFromScreen/);
  assert.match(liveExercise, /Get-Content[\s\S]{0,120}-Tail/);
  assert.match(liveExercise, /TimeoutSeconds/);
  assert.match(liveExercise, /cursor placement started/);
});

test('the display indicator exercise compares local and received pixels', () => {
  assert.match(displayExercise, /CopyFromScreen/);
  assert.match(displayExercise, /EdgeColorFraction/);
  assert.match(displayExercise, /ReceivedFramePath/);
  assert.match(displayExercise, /local.*received/s);
  assert.match(displayExercise, /Get-Content[\s\S]{0,120}-Tail/);
});

test('the overlay tracking exercise drives geometry, activation, and bounded metrics', () => {
  assert.match(trackingExercise, /DwmGetWindowAttribute/);
  assert.match(trackingExercise, /SendInput/);
  assert.match(trackingExercise, /continuous-move/);
  assert.match(trackingExercise, /move-\$\{delta\}px/);
  assert.match(trackingExercise, /SW_MAXIMIZE/);
  assert.match(trackingExercise, /SW_MINIMIZE/);
  assert.match(trackingExercise, /titlebar-activation/);
  assert.match(trackingExercise, /HybridRectangles/);
  assert.match(trackingExercise, /FrontendFirstPaintDelta/);
  assert.match(trackingExercise, /MaxEdgeErrorPx/);
  assert.match(trackingExercise, /GW_OWNER/);
  assert.match(trackingExercise, /OwnerHwnd/);
  assert.match(trackingExercise, /-Mode/);
  assert.match(trackingExercise, /Owned|Passive/);
  assert.match(trackingExercise, /StackingOkay/);
  assert.match(trackingExercise, /SystemIndicatorLogSeen/);
  assert.match(trackingExercise, /PassiveCustomReadinessSeen/);
  assert.match(trackingExercise, /OccluderWindowTitle/);
  assert.match(trackingExercise, /AboveOverlay/);
});

test('idle Petal View remains recordable while active sharing is excluded', () => {
  assert.match(platformWindowsSource, /set_capture_affinity/);
  assert.match(platformWindowsSource, /WDA_NONE/);
  assert.match(platformWindowsSource, /WDA_EXCLUDEFROMCAPTURE/);
  assert.match(regionNativeSource, /WDA_NONE|clear_capture_exclusion/);
  assert.match(regionNativeSource, /WDA_EXCLUDEFROMCAPTURE/);
  assert.match(architecture, /starts with `WDA_NONE`/);
  assert.match(architecture, /active display-region share holds a scoped `WDA_EXCLUDEFROMCAPTURE` lease/);
  assert.match(audit, /`WDA_NONE` while idle/);
  assert.match(audit, /label-owned `WDA_EXCLUDEFROMCAPTURE` lease/);
  assert.match(audit, /idle → active → idle/);
});

test('the Windows native-surface inventory has one audited row per surface', () => {
  const requiredRows = [
    'main',
    'hover-tab',
    'window-picker',
    'network-cockpit',
    'region-window-*',
    'petal-sharer-pointer-*',
    'petal-remote-*',
    'petal-control-*',
    'petal-pointer-*',
    'ai-chat-panel'
  ];
  const matrix = audit.split('## Decisions locked by this work')[0];
  for (const row of requiredRows) {
    assert.ok(architecture.includes('| `' + row + '`'), `${row} is missing from the architecture inventory`);
    const rowPrefix = '| `' + row + '`';
    assert.equal(
      matrix.split('\n').filter((line) => line.startsWith(rowPrefix)).length,
      1,
      `${row} must appear exactly once in the audit matrix`
    );
  }
});
