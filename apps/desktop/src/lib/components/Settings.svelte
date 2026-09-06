<!--
  Settings — cross-cutting settings panel. Per DESIGN.md §9 ("Settings:
  devices (camera/mic/speaker selection + preview), permissions re-entry,
  account") and Petal-Build-Map.md §4 ("§9 Settings / device
  picker / roster popover — not shown [in the approved canvas]"). EXPLICITLY
  UNDESIGNED — confirmed no "settings"/"device"/"appearance" section exists
  anywhere in canvas.html. The device controls use Petal-native graphite
  popovers rather than browser/OS select menus, while reusing the app's
  raised surfaces, hairlines, typography, focus treatment, and motion.

  Sections, per the task brief:
  1. Devices — camera/mic/speaker menus + a REAL camera preview
     (getUserMedia in the main webview; falls back to the quiet placeholder
     with a muted reason on denial/no-camera). The camera menu is fed real
     enumerated videoinput devices once permission is granted. Mic/speaker
     menus are fed the REAL native device lists (issue #28 —
     `list_audio_devices`, riding livekit's PlatformAudio) and selecting one
     records the preference + hot-swaps the live mic track / playout device
     when in a room (`set_audio_devices`), with an honest one-line status
     under each menu ("saved, applies when you join" vs. switched vs. a
     real error). Static sample options remain only as the no-Tauri-bridge
     (plain browser harness) fallback.
  2. Permissions re-entry — reuses `PermissionRow` as-is (not duplicated)
     per SPEC.md §4.1 ("always recoverable later... surface the same setup
     flow from Settings"). Same collapsed enabled/skipped rows Onboarding
     already renders, so re-checking permissions from Settings looks
     identical to the onboarding checklist's own "enabled" state. The whole
     section is macOS-gated: Windows has no TCC permission model, the
     permission stubs always report granted, and the rows would be dead UI.
  3. Account — re-opens `IdentitySetup`'s color picker inline (same
     component instance type Onboarding uses, not a second copy of the
     swatch palette).
     Appearance is intentionally absent for now (issue #90); dark-only
     remains the v1 stance.

  Color-rationing check: this entire panel stays graphite. No LiveHero-style
  bloom, no identity color anywhere except inside the reused IdentitySetup
  avatar/swatches (the one sanctioned identity-color surface) — a settings
  screen is exactly the kind of secondary surface Build-Map §1/§3 warns is
  easy to accidentally decorate with color that doesn't belong.
-->
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import { writeText } from '@tauri-apps/plugin-clipboard-manager';
  import Button from './Button.svelte';
  import Switch from '@petal/shared/ui/components/Switch.svelte';
  import {
    REMOTE_CONTROL_POLICY_DESCRIPTION,
    REMOTE_CONTROL_POLICY_OPTIONS,
    REMOTE_CONTROL_POLICY_TITLE
  } from '$lib/remoteControlPolicyCopy';
  import type { RemoteControlPolicy } from '$lib/ipc';
  import DeviceSelect from './DeviceSelect.svelte';
  import IdentitySetup from './IdentitySetup.svelte';
  import PermissionRow from './PermissionRow.svelte';
  import TestCockpitResults from './TestCockpitResults.svelte';
  import type { PermissionStatus } from './PermissionRow.svelte';
  import type { IdentityColor } from './Avatar.svelte';
  import {
    ensureCameraAccess,
    openPrivacySettings,
    type AuthStatus
  } from '$lib/data/permissions';
  import { cameraPreviewConstraints } from '$lib/data/cameraConstraints';
  import { listAudioDevices, setAudioDevices } from '$lib/data/audioDevices';
  import { listCameraDevices, setCameraDevice } from '$lib/data/cameraDevices';
  import {
    CAMERA_FPS_PRESETS,
    CAMERA_RESOLUTION_PRESETS,
    cameraSupportsFps,
    cameraSupportsResolution,
    listCameraModes,
    setCameraPrefs
  } from '$lib/data/cameraModes';
  import type { CameraMode } from '$lib/data/cameraModes';
  import { openUrl } from '@tauri-apps/plugin-opener';
  import {
    AI_CHAT_API_KEY_URL,
    AI_CHAT_CONSENT_DESCRIPTION,
    AI_CHAT_CONSENT_SHARED_WINDOW_WARNING,
    AI_CHAT_CONSENT_TITLE,
    AI_CHAT_COST_NOTE,
    AI_CHAT_KEY_DATA_USE_NOTE
  } from '$lib/data/aiChat';
  import { DEBUG_MODE_SETTING_DESCRIPTION, DEBUG_MODE_SETTING_TITLE } from '$lib/data/debugMode';
  import {
    COMMANDS,
    EVENTS,
    hasTauriBridge,
    type AiChatSettings,
    type BuildInfo,
    type CockpitJourney,
    type CockpitStatus,
    type DebugModeSettings,
    type TestCockpitEvent,
    type TestCockpitRunDetail,
    type TestCockpitRunSummary,
    type TestProgressEvent
  } from '$lib/ipc';
  import { displayBuildVersion } from '$lib/buildInfo';
  import { isMac } from '$lib/platform';
  import { resetOnboarding, session, updateAudioDevices, updateCameraMode } from '$lib/stores/session.svelte';
  import { checkForUpdate } from '$lib/updater';
  import { clearLocalFactoryResetState, tccResetCommand } from '$lib/data/factoryReset';
  import {
    cockpitSummaryLine,
    latestCockpitMessage,
    cockpitFeatureGroups,
    journeyIsRunnable,
    journeySelector,
    featureSelector,
    directionLabel,
    depthLabel,
    journeyStatusInfo,
    verdictToLiveState,
    JOURNEY_LIVE_LABELS,
    COCKPIT_PRESETS,
    type JourneyLiveState
  } from '$lib/data/testCockpit';

  interface DeviceOption {
    id: string;
    label: string;
  }

  interface Props {
    userName?: string;
    identity?: IdentityColor;
    cameras?: DeviceOption[];
    mics?: DeviceOption[];
    speakers?: DeviceOption[];
    selectedCamera?: string;
    selectedMic?: string;
    selectedSpeaker?: string;
    screenRecordingStatus?: PermissionStatus;
    micStatus?: PermissionStatus;
    cameraStatus?: PermissionStatus;
    accessibilityStatus?: PermissionStatus;
    onNameChange?: (name: string) => void;
    onIdentityChange?: (identity: IdentityColor) => void;
    remoteControlPolicy?: RemoteControlPolicy;
    onRemoteControlPolicyChange?: (policy: RemoteControlPolicy) => void;
    localEchoEnabled?: boolean;
    onLocalEchoEnabledChange?: (enabled: boolean) => void;
    sentryEnabled?: boolean;
    onSentryEnabledChange?: (enabled: boolean) => void;
    onOpenSettings?: () => void;
    /** Real routes pass true so the panel IS the window (edge-to-edge, no
     * fixed-size floating card). Default false preserves the card look for
     * the /dev/* harnesses. */
    frameless?: boolean;
  }

  let {
    userName = $bindable('Guest'),
    identity = $bindable('plum'),
    cameras = [{ id: 'builtin', label: 'Built-in Camera' }],
    mics = [{ id: 'builtin', label: 'Built-in Microphone' }],
    speakers = [{ id: 'default', label: 'Built-in Speakers' }],
    selectedCamera = $bindable(''),
    selectedMic = $bindable(''),
    selectedSpeaker = $bindable(''),
    screenRecordingStatus = 'enabled',
    micStatus = 'enabled',
    cameraStatus = 'enabled',
    accessibilityStatus = 'enabled',
    onNameChange,
    onIdentityChange,
    remoteControlPolicy = 'ask',
    onRemoteControlPolicyChange,
    localEchoEnabled = false,
    onLocalEchoEnabledChange,
    sentryEnabled = true,
    onSentryEnabledChange,
    onOpenSettings,
    frameless = false
  }: Props = $props();

  // Browser camera ids are retained only for best-effort preview acquisition;
  // the picker itself uses native AVFoundation ids from list_camera_devices.
  let realCameras = $state<DeviceOption[]>([]);
  let nativeCameras = $state<DeviceOption[]>([]);

  // User-chosen camera capture mode (Settings resolution/FPS menus). The
  // modes list is enumerated per camera so unsupported presets are greyed
  // out. Windows-only: the macOS camera path has no resolution/FPS prefs.
  let cameraModes = $state<CameraMode[]>([]);
  let cameraModesError = $state(false);
  let cameraResolutionId = $state('auto');
  let cameraFps = $state(30);
  let cameraPrefsNote = $state<string | null>(null);
  let cameraPrefsSeeded = false;

  // Real enumerated mic/speaker devices (issue #28 — native-side
  // `list_audio_devices`, not the browser's enumerateDevices, since the
  // published mic track lives on the Rust ADM). Loaded once on mount when a
  // Tauri backend exists; static sample options remain the plain-browser
  // fallback. `audioDevicesError` carries the backend's honest failure
  // string ("audio devices unavailable: ...").
  let realMics = $state<DeviceOption[]>([]);
  let realSpeakers = $state<DeviceOption[]>([]);
  let audioDevicesLoaded = $state(false);
  let audioDevicesError = $state<string | null>(null);
  let cameraDevicesLoaded = $state(false);
  let cameraDevicesError = $state<string | null>(null);
  // One-line honest status under each select after a selection: switched
  // live, saved-for-next-join, or a real error. Null = nothing to report.
  let micNote = $state<string | null>(null);
  let speakerNote = $state<string | null>(null);
  let cameraNote = $state<string | null>(null);
  let exportLogsBusy = $state(false);
  let exportLogsNote = $state<string | null>(null);
  let exportLogsError = $state<string | null>(null);
  // #905: date range for the export -- 2 (default, matches the backend's
  // own default when this arg is omitted), 7, or 0 (sentinel for "all logs,
  // no filtering" -- see `logging::export_logs`'s doc comment).
  let exportLogsDays = $state<number>(2);

  let buildInfo = $state<BuildInfo | null>(null);
  let checkUpdatesBusy = $state(false);
  let checkUpdatesNote = $state<string | null>(null);
  let checkUpdatesError = $state<string | null>(null);
  let cockpitBusy = $state(false);
  let cockpitStatus = $state<CockpitStatus | null>(null);
  let cockpitProgress = $state<TestProgressEvent[]>([]);
  let cockpitError = $state<string | null>(null);
  let cockpitResultsNote = $state<string | null>(null);
  let cockpitRuns = $state<TestCockpitRunSummary[]>([]);
  let selectedCockpitRun = $state<TestCockpitRunDetail | null>(null);
  let cockpitRunsLoading = $state(false);
  let cockpitRunsError = $state<string | null>(null);
  // Which selector is in flight (so only the button that launched it shows a
  // spinner) + the journey ids that launch targeted (drives the live "queued"
  // marker — we know the scope because we built the selector).
  let runningSelector = $state<string | null>(null);
  let launchedScope = $state<Set<string>>(new Set());
  // Feature groups the user has collapsed (default: all expanded).
  let collapsedFeatures = $state<Set<string>>(new Set());
  const cockpitFeatureGroupsView = cockpitFeatureGroups();
  // AI chat (#656). The key is WRITE-ONLY: `ai_chat_settings` returns
  // `hasApiKey`, never the value (mirrors Rust's `settings::Redacted`), so this
  // component can only ever render "Key saved" — there is nothing to read back.
  let aiChat = $state<AiChatSettings>({ enabled: false, hasApiKey: false });
  let aiChatLoaded = $state(false);
  let aiChatKeyDraft = $state('');
  let aiChatBusy = $state(false);
  let aiChatNote = $state<string | null>(null);
  let aiChatError = $state<string | null>(null);

  // Debug mode (#669): gates the remote-window header's Debug button.
  // Rust-owned, not localStorage -- see debugMode.ts / debug_settings.rs.
  let debugSettings = $state<DebugModeSettings>({ enabled: false });
  let debugSettingsLoaded = $state(false);
  let debugModeError = $state<string | null>(null);

  let resetConfirmOpen = $state(false);
  let resetBusy = $state(false);
  let resetNote = $state<string | null>(null);
  // Confirm popover (anchored under the Reset button, overlays the page so
  // opening it never shifts the settings layout). `resetOpenAbove` flips the
  // popover upward when it would overflow the settings scrollport below.
  let resetActionsRoot = $state<HTMLElement | null>(null);
  let resetOpenAbove = $state(false);

  function toggleResetConfirm() {
    if (resetConfirmOpen) {
      resetConfirmOpen = false;
      return;
    }
    // The popover is always mounted (visibility-hidden), so its height is
    // measurable BEFORE revealing it: flip the open direction first, then
    // open with the entrance transition already correct from frame one.
    const root = resetActionsRoot;
    const popover = root?.querySelector<HTMLElement>('.reset-popover');
    if (root && popover) {
      const bounds = root.getBoundingClientRect();
      const popoverHeight = popover.getBoundingClientRect().height;
      const scrollport = root.closest('.settings-body')?.getBoundingClientRect();
      const spaceBelow = (scrollport?.bottom ?? window.innerHeight) - bounds.bottom - 6;
      resetOpenAbove = popoverHeight > spaceBelow;
    }
    resetConfirmOpen = true;
  }

  function handleResetKeydown(event: KeyboardEvent) {
    if (event.key === 'Escape' && resetConfirmOpen) {
      event.preventDefault();
      resetConfirmOpen = false;
    }
  }

  function handleResetFocusOut(event: FocusEvent) {
    if (
      resetConfirmOpen &&
      (!(event.relatedTarget instanceof Node) || !resetActionsRoot?.contains(event.relatedTarget))
    ) {
      resetConfirmOpen = false;
    }
  }
  let resetError = $state<string | null>(null);
  let resetCopyFailedConfirm = $state(false);

  const micOptions = $derived(realMics.length > 0 ? realMics : mics);
  const speakerOptions = $derived(realSpeakers.length > 0 ? realSpeakers : speakers);
  const cameraOptions = $derived(
    cameraDevicesLoaded && !cameraDevicesError ? nativeCameras : cameras
  );
  // Honest empty state: a real backend answered and the machine genuinely
  // has zero devices of that kind (distinct from "no backend, sample data").
  const noMics = $derived(audioDevicesLoaded && realMics.length === 0 && !audioDevicesError);
  const noSpeakers = $derived(
    audioDevicesLoaded && realSpeakers.length === 0 && !audioDevicesError
  );
  const noCameras = $derived(
    cameraDevicesLoaded && nativeCameras.length === 0 && !cameraDevicesError
  );

  // Which resolution/FPS presets the selected camera can actually deliver;
  // everything else is greyed out in the menus.
  const selectedResolution = $derived(
    CAMERA_RESOLUTION_PRESETS.find((p) => p.id === cameraResolutionId) ??
      CAMERA_RESOLUTION_PRESETS[0]
  );
  const cameraResolutionDisabled = $derived(
    new Set(
      CAMERA_RESOLUTION_PRESETS.filter(
        (p) => p.id !== 'auto' && !cameraSupportsResolution(cameraModes, p.width, p.height)
      ).map((p) => p.id)
    )
  );
  const cameraFpsDisabled = $derived(
    selectedResolution.id === 'auto'
      ? new Set(CAMERA_FPS_PRESETS.map(String))
      : new Set(
          CAMERA_FPS_PRESETS.filter(
            (fps) =>
              !cameraSupportsFps(
                cameraModes,
                selectedResolution.width,
                selectedResolution.height,
                fps
              )
          ).map(String)
        )
  );
  const cameraModesAvailable = $derived(
    cameraDevicesLoaded && !cameraDevicesError && cameraModes.length > 0
  );
  const hasDisabledCameraPresets = $derived(
    cameraResolutionDisabled.size > 0 || cameraFpsDisabled.size > 0
  );

  // Default the selects to the first sample option if the caller didn't
  // pass a specific selection, so the dropdowns never render empty. Derived
  // (not a mutation of the bindable prop itself) so it stays reactive to
  // late-arriving device lists without the "state referenced locally" pitfall.
  // Mic/speaker additionally fall back to the first option when the persisted
  // selection no longer exists (device unplugged since it was saved).
  const cameraValue = $derived(
    cameraOptions.some((c) => c.id === selectedCamera)
      ? selectedCamera
      : (cameraOptions[0]?.id ?? '')
  );
  const micValue = $derived(
    micOptions.some((m) => m.id === selectedMic) ? selectedMic : (micOptions[0]?.id ?? '')
  );
  const speakerValue = $derived(
    speakerOptions.some((s) => s.id === selectedSpeaker)
      ? selectedSpeaker
      : (speakerOptions[0]?.id ?? '')
  );
  const showTestCockpit = $derived(Boolean(buildInfo?.cockpitPrivileged));
  const cockpitMessage = $derived(latestCockpitMessage(cockpitStatus, cockpitProgress));
  const cockpitSummary = $derived(cockpitSummaryLine(cockpitStatus?.summary));
  const cockpitRunning = $derived(Boolean(cockpitStatus?.running));

  // The scenario currently being driven (live), taken from the newest
  // `phase: "scenario"` progress event while a run is active.
  const runningScenarioId = $derived.by(() => {
    if (!cockpitStatus?.running) return null;
    for (let i = cockpitProgress.length - 1; i >= 0; i -= 1) {
      const event = cockpitProgress[i];
      if (event.phase === 'scenario' && event.scenarioId) return event.scenarioId;
    }
    return null;
  });

  // Every scenario id that has begun in the active run (used to tell "queued"
  // from "already ran" on rows that share a scenario).
  const startedScenarioIds = $derived.by(() => {
    const set = new Set<string>();
    for (const event of cockpitProgress) {
      if (event.phase === 'scenario' && event.scenarioId) set.add(event.scenarioId);
    }
    return set;
  });

  // Pass/fail/skip pulled from the selected run's `scenario-verdict` records,
  // keyed both by journey id and by scenario id so journeys that share a
  // scenario (e.g. SHARE-02/SHARE-03 → SHARE-W2N-Q) both resolve a verdict.
  const cockpitVerdicts = $derived.by(() => {
    const map = new Map<string, JourneyLiveState>();
    const events: TestCockpitEvent[] = selectedCockpitRun?.events ?? [];
    for (const event of events) {
      if (event.kind !== 'scenario-verdict') continue;
      const payload = (event.payload ?? null) as Record<string, unknown> | null;
      const rawVerdict = typeof payload?.verdict === 'string' ? payload.verdict : null;
      if (!rawVerdict) continue;
      const state = verdictToLiveState(rawVerdict);
      if (!state) continue;
      const journeyId = typeof payload?.journeyId === 'string' ? payload.journeyId : null;
      if (journeyId) map.set(journeyId, state);
      const scenarioId =
        event.scenarioId ??
        (typeof payload?.scenarioId === 'string' ? (payload.scenarioId as string) : null);
      if (scenarioId) map.set(`scenario:${scenarioId}`, state);
    }
    return map;
  });
  const selectedCockpitRunView = $derived(
    selectedCockpitRun
      ? {
          ...selectedCockpitRun.summary,
          events: selectedCockpitRun.events,
          artifacts: selectedCockpitRun.artifacts,
          scorecard: selectedCockpitRun.scorecard
        }
      : null
  );
  const permissionResetCommand = $derived(tccResetCommand(buildInfo?.bundleIdentifier ?? 'com.petal.app'));

  async function handleMicSelect(id: string) {
    selectedMic = id;
    updateAudioDevices(id, undefined);
    micNote = null;
    try {
      const applied = await setAudioDevices({ recordingId: id });
      if (!applied) return; // plain browser — nothing real to apply
      if (applied.micApplied) micNote = 'Switched microphone';
      else if (!applied.inRoom) micNote = 'Saved — applies when you join a room';
      else if (applied.micError === 'no live microphone track')
        micNote = 'Saved — microphone isn’t active in this meeting (check mic permission)';
      else micNote = applied.micError ?? 'Could not switch microphone';
    } catch (e) {
      micNote = `Could not switch microphone: ${e}`;
    }
  }

  async function handleSpeakerSelect(id: string) {
    selectedSpeaker = id;
    updateAudioDevices(undefined, id);
    speakerNote = null;
    try {
      const applied = await setAudioDevices({ playoutId: id });
      if (!applied) return;
      if (applied.speakerApplied) speakerNote = 'Switched speaker';
      else if (!applied.inRoom) speakerNote = 'Saved — applies when you join a room';
      else speakerNote = applied.speakerError ?? 'Could not switch speaker';
    } catch (e) {
      speakerNote = `Could not switch speaker: ${e}`;
    }
  }

  async function handleCameraSelect(id: string) {
    selectedCamera = id;
    updateAudioDevices(undefined, undefined, id);
    cameraNote = null;
    void refreshCameraModes(id);
    try {
      const applied = await setCameraDevice(id);
      if (!applied) {
        void acquirePreview(id);
        return;
      }
      if (applied.usedDefaultFallback) cameraNote = 'Camera not found, using default';
      else if (applied.applied) cameraNote = 'Switched camera';
      else if (!applied.inRoom) cameraNote = 'Saved — applies when you join a room';
      else if (applied.error) cameraNote = `Could not switch camera: ${applied.error}`;
      else cameraNote = 'Saved — applies when you enable the camera';
    } catch (e) {
      cameraNote = `Could not switch camera: ${e}`;
    }
    // The native id space cannot be safely correlated with WebKit's salted
    // deviceId space. The preview therefore remains best-effort/default.
    void acquirePreview(id);
  }

  async function refreshCameraModes(deviceId?: string) {
    if (!hasTauri || isMac()) return;
    const id = deviceId ?? cameraValue;
    if (!id) return;
    if (!cameraPrefsSeeded) {
      cameraPrefsSeeded = true;
      const m = session.cameraMode;
      if (m) {
        const preset = CAMERA_RESOLUTION_PRESETS.find(
          (p) => p.width === m.width && p.height === m.height
        );
        if (preset) {
          cameraResolutionId = preset.id;
          if (CAMERA_FPS_PRESETS.includes(m.frameRate)) cameraFps = m.frameRate;
        }
      }
    }
    try {
      cameraModes = (await listCameraModes(id)) ?? [];
      cameraModesError = false;
    } catch (e) {
      cameraModes = [];
      cameraModesError = true;
      console.warn('refreshCameraModes failed', e);
    }
    // A selection this camera can't deliver falls back to Auto — the capture
    // side would fall back too, so keep the UI honest.
    const res = CAMERA_RESOLUTION_PRESETS.find((p) => p.id === cameraResolutionId);
    if (res && res.id !== 'auto') {
      if (!cameraSupportsResolution(cameraModes, res.width, res.height)) {
        cameraResolutionId = 'auto';
        void applyCameraPrefs();
      } else if (!cameraSupportsFps(cameraModes, res.width, res.height, cameraFps)) {
        const first = CAMERA_FPS_PRESETS.find((fps) =>
          cameraSupportsFps(cameraModes, res.width, res.height, fps)
        );
        if (first !== undefined) cameraFps = first;
        void applyCameraPrefs();
      }
    }
  }

  async function applyCameraPrefs() {
    if (!hasTauri || isMac()) return;
    const res =
      CAMERA_RESOLUTION_PRESETS.find((p) => p.id === cameraResolutionId) ??
      CAMERA_RESOLUTION_PRESETS[0];
    cameraPrefsNote = null;
    const width = res.id === 'auto' ? null : res.width;
    const height = res.id === 'auto' ? null : res.height;
    const frameRate = res.id === 'auto' ? null : cameraFps;
    try {
      const applied = await setCameraPrefs(width, height, frameRate);
      if (applied) {
        if (applied.applied) cameraPrefsNote = 'Switched camera mode';
        else if (!applied.inRoom) cameraPrefsNote = 'Saved — applies when you join a room';
        else if (applied.error) cameraPrefsNote = `Could not switch camera mode: ${applied.error}`;
        else cameraPrefsNote = 'Saved — applies when you enable the camera';
      }
    } catch (e) {
      cameraPrefsNote = `Could not switch camera mode: ${e}`;
    }
    updateCameraMode(
      res.id === 'auto' ? null : { width: res.width, height: res.height, frameRate: cameraFps }
    );
  }

  async function handleResolutionSelect(id: string) {
    cameraResolutionId = id;
    if (id !== 'auto') {
      const res = CAMERA_RESOLUTION_PRESETS.find((p) => p.id === id);
      if (res && !cameraSupportsFps(cameraModes, res.width, res.height, cameraFps)) {
        const first = CAMERA_FPS_PRESETS.find((fps) =>
          cameraSupportsFps(cameraModes, res.width, res.height, fps)
        );
        if (first !== undefined) cameraFps = first;
      }
    }
    await applyCameraPrefs();
  }

  async function handleFpsSelect(fps: string) {
    cameraFps = Number(fps);
    await applyCameraPrefs();
  }

  // ---- AI chat (#656) -------------------------------------------------
  // Every mutation adopts the command's returned settings rather than
  // optimistically flipping local state: Rust is the store of record, and
  // `ai_chat_set_enabled(false)` also stops any running session, so a local
  // guess could disagree with what actually happened.

  async function handleAiChatEnabledChange(enabled: boolean) {
    aiChatNote = null;
    aiChatError = null;
    const previous = aiChat;
    aiChat = { ...aiChat, enabled };
    if (!hasTauri) return;
    try {
      aiChat = await invoke<AiChatSettings>(COMMANDS.aiChatSetEnabled, { enabled });
    } catch (e) {
      aiChat = previous;
      aiChatError = `Could not change the AI chat setting: ${e}`;
    }
  }

  // ---- Debug mode (#669) ------------------------------------------------
  // Same adopt-the-command's-returned-settings shape as AI chat above: Rust
  // is the store of record. `set_debug_mode` also emits `debug-mode-changed`
  // so any already-open remote-window header updates live -- this handler
  // doesn't need to do anything extra for that; it's purely Rust-side.
  async function handleDebugModeEnabledChange(enabled: boolean) {
    debugModeError = null;
    const previous = debugSettings;
    debugSettings = { ...debugSettings, enabled };
    if (!hasTauri) return;
    try {
      debugSettings = await invoke<DebugModeSettings>(COMMANDS.setDebugMode, { enabled });
    } catch (e) {
      debugSettings = previous;
      debugModeError = `Could not change the debug mode setting: ${e}`;
    }
  }

  async function handleAiChatSaveKey() {
    const key = aiChatKeyDraft.trim();
    if (!key || aiChatBusy) return;
    aiChatBusy = true;
    aiChatNote = null;
    aiChatError = null;
    try {
      aiChat = await invoke<AiChatSettings>(COMMANDS.aiChatSetApiKey, { key });
      aiChatKeyDraft = '';
      aiChatNote = 'Key saved';
    } catch (e) {
      aiChatError = `Could not save the key: ${e}`;
    } finally {
      aiChatBusy = false;
    }
  }

  async function handleAiChatRemoveKey() {
    if (aiChatBusy) return;
    aiChatBusy = true;
    aiChatNote = null;
    aiChatError = null;
    try {
      aiChat = await invoke<AiChatSettings>(COMMANDS.aiChatSetApiKey, { key: null });
      aiChatKeyDraft = '';
      aiChatNote = 'Key removed';
    } catch (e) {
      aiChatError = `Could not remove the key: ${e}`;
    } finally {
      aiChatBusy = false;
    }
  }

  async function openAiChatKeyPage() {
    try {
      await openUrl(AI_CHAT_API_KEY_URL);
    } catch {
      // Plain-browser harness (no opener plugin) — best-effort fallback.
      window.open(AI_CHAT_API_KEY_URL, '_blank', 'noopener');
    }
  }

  async function handleExportLogs() {
    exportLogsBusy = true;
    exportLogsNote = null;
    exportLogsError = null;
    try {
      const result = await invoke<{
        archivePath: string;
        fileCount: number;
        revealed: boolean;
      }>(COMMANDS.exportLogs, { days: exportLogsDays });
      exportLogsNote = result.revealed
        ? `Revealed ${result.fileCount} log file${result.fileCount === 1 ? '' : 's'}`
        : `Saved log zip: ${result.archivePath}`;
    } catch (e) {
      exportLogsError = `Could not export logs: ${e}`;
    } finally {
      exportLogsBusy = false;
    }
  }

  async function refreshCockpitStatus() {
    if (!hasTauri || !showTestCockpit) return;
    try {
      cockpitStatus = await invoke<CockpitStatus>(COMMANDS.cockpitStatus);
    } catch (e) {
      cockpitError = `Could not load cockpit status: ${e}`;
    }
  }

  async function refreshCockpitRuns(selectLatest = false) {
    if (!hasTauri || !showTestCockpit) return;
    cockpitRunsLoading = true;
    cockpitRunsError = null;
    try {
      const runs = await invoke<TestCockpitRunSummary[]>(COMMANDS.listTestCockpitRuns);
      cockpitRuns = runs;
      const currentStillExists =
        selectedCockpitRun && runs.some((run) => run.runId === selectedCockpitRun?.summary.runId);
      if (selectLatest && runs[0]) {
        await handleSelectCockpitRun(runs[0].runId, runs);
      } else if (!currentStillExists && runs[0]) {
        await handleSelectCockpitRun(runs[0].runId, runs);
      } else if (!runs[0]) {
        selectedCockpitRun = null;
      }
    } catch (e) {
      cockpitRunsError = `Could not load cockpit runs: ${e}`;
    } finally {
      cockpitRunsLoading = false;
    }
  }

  async function handleSelectCockpitRun(runId: string, sourceRuns = cockpitRuns) {
    const run = sourceRuns.find((candidate) => candidate.runId === runId);
    if (!run) return;
    cockpitRunsLoading = true;
    cockpitRunsError = null;
    try {
      selectedCockpitRun = await invoke<TestCockpitRunDetail>(COMMANDS.getTestCockpitRun, {
        resultsDir: run.resultsDir
      });
    } catch (e) {
      cockpitRunsError = `Could not load cockpit run: ${e}`;
    } finally {
      cockpitRunsLoading = false;
    }
  }

  async function handleRunCockpit(selector: string, scope: Set<string>) {
    if (cockpitBusy || cockpitStatus?.running) return;
    cockpitBusy = true;
    cockpitError = null;
    cockpitResultsNote = null;
    cockpitProgress = [];
    runningSelector = selector;
    launchedScope = scope;
    try {
      cockpitStatus = await invoke<CockpitStatus>(COMMANDS.startTestCockpit, {
        args: { selector }
      });
      void refreshCockpitRuns(true);
    } catch (e) {
      cockpitError = `Could not start cockpit: ${e}`;
      runningSelector = null;
    } finally {
      cockpitBusy = false;
    }
  }

  function runJourney(journey: CockpitJourney) {
    if (!journeyIsRunnable(journey)) return;
    void handleRunCockpit(journeySelector(journey), new Set([journey.id]));
  }

  function runFeature(group: (typeof cockpitFeatureGroupsView)[number]) {
    if (group.runnableCount === 0) return;
    const scope = new Set(
      group.journeys.filter((journey) => journeyIsRunnable(journey)).map((journey) => journey.id)
    );
    void handleRunCockpit(featureSelector(group), scope);
  }

  function runPreset(preset: (typeof COCKPIT_PRESETS)[number]) {
    void handleRunCockpit(preset.selector, presetScope(preset.id));
  }

  function presetScope(presetId: string): Set<string> {
    const isShort = (depth: string) => depth === 'short' || depth === 'short-long';
    const isLong = (depth: string) => depth === 'long' || depth === 'short-long';
    const runnable = cockpitFeatureGroupsView
      .flatMap((group) => group.journeys)
      .filter((journey) => journeyIsRunnable(journey));
    let rows: CockpitJourney[] = [];
    if (presetId === 'quick') rows = runnable.filter((j) => j.priority === 'P0' && isShort(j.depth));
    else if (presetId === 'full') rows = runnable.filter((j) => isShort(j.depth));
    else if (presetId === 'soak') rows = runnable.filter((j) => isLong(j.depth));
    return new Set(rows.map((j) => j.id));
  }

  function toggleFeature(code: string) {
    const next = new Set(collapsedFeatures);
    if (next.has(code)) next.delete(code);
    else next.add(code);
    collapsedFeatures = next;
  }

  function journeyLiveState(journey: CockpitJourney): JourneyLiveState {
    if (!journeyIsRunnable(journey)) return null;
    const runnable = journey.runnable as string;
    const verdict =
      cockpitVerdicts.get(journey.id) ?? cockpitVerdicts.get(`scenario:${runnable}`) ?? null;
    if (cockpitRunning) {
      if (runningScenarioId === runnable) return 'running';
      if (launchedScope.has(journey.id)) {
        // Verdicts stream in as the run's earlier scenarios finish; until then
        // an in-scope, not-yet-started journey is queued.
        if (startedScenarioIds.has(runnable)) return verdict;
        return 'queued';
      }
    }
    return verdict;
  }

  async function handleCancelCockpit() {
    cockpitBusy = true;
    cockpitError = null;
    try {
      cockpitStatus = await invoke<CockpitStatus>(COMMANDS.cancelTestCockpit);
    } catch (e) {
      cockpitError = `Could not cancel cockpit: ${e}`;
    } finally {
      cockpitBusy = false;
    }
  }

  async function handleOpenCockpitResults(path = cockpitStatus?.resultsDir ?? null) {
    if (!path) return;
    cockpitResultsNote = null;
    cockpitError = null;
    try {
      const opened = await invoke<boolean>(COMMANDS.openTestCockpitResultsFolder, { path });
      cockpitResultsNote = opened ? 'Opened results folder' : `Results folder unavailable: ${path}`;
    } catch (e) {
      cockpitError = `Could not open results folder: ${e}`;
    }
  }

  async function copyPermissionResetCommand() {
    try {
      await writeText(permissionResetCommand);
      resetNote = 'Copied permission reset commands';
      return true;
    } catch {
      try {
        await navigator.clipboard.writeText(permissionResetCommand);
        resetNote = 'Copied permission reset commands';
        return true;
      } catch {
        resetNote = 'Could not copy automatically; copy the commands below';
        return false;
      }
    }
  }

  async function handleFactoryReset(skipCopy = false) {
    resetBusy = true;
    resetError = null;
    // The tccutil commands are macOS-only (no TCC on Windows), so the
    // copy-before-quit gate only applies on macOS; Windows resets directly.
    if (!skipCopy && isMac()) {
      resetNote = null;
      resetCopyFailedConfirm = false;
      const copied = await copyPermissionResetCommand();
      if (!copied) {
        // Don't quit silently on a failed copy -- the commands below are the
        // only way to also reset macOS permissions, and once Petal quits
        // there's no going back to retry the copy. Require an explicit
        // "Quit anyway" instead (#270 follow-up).
        resetBusy = false;
        resetCopyFailedConfirm = true;
        return;
      }
    }
    try {
      if (hasTauri) {
        await invoke(COMMANDS.leaveRoom).catch(() => {});
        await invoke(COMMANDS.resetLocalRooms);
      }
      resetOnboarding();
      clearLocalFactoryResetState();
      resetNote = 'Local state reset. Petal will quit.';
      if (hasTauri) await invoke(COMMANDS.quitApp);
    } catch (e) {
      resetError = `Could not reset Petal: ${e}`;
    } finally {
      resetBusy = false;
    }
  }

  async function handleCheckForUpdates() {
    checkUpdatesBusy = true;
    checkUpdatesNote = null;
    checkUpdatesError = null;
    try {
      // Calling checkForUpdate() directly (not through +layout.svelte's
      // runUpdateCheck) bypasses that check's 30-minute throttle — this is
      // the deliberate "I want to know right now" escape hatch (#188), not
      // a change to the passive launch/main-menu cadence.
      const result = await checkForUpdate({ skipRelaunch: true, reason: 'manual' });
      switch (result.status) {
        case 'up-to-date':
          checkUpdatesNote = 'You’re on the latest version';
          break;
        case 'available':
          checkUpdatesNote = `Update ${result.version ?? ''} ready — use the Restart now toast to install`.trim();
          break;
        case 'installed':
          checkUpdatesNote = `Update ${result.version ?? ''} installed — relaunching`.trim();
          break;
        case 'unavailable':
          checkUpdatesNote = 'Updates aren’t available in this build';
          break;
        case 'error':
          checkUpdatesError = result.error ? `Could not check for updates: ${result.error}` : 'Could not check for updates';
          break;
      }
    } finally {
      checkUpdatesBusy = false;
    }
  }

  // Real camera preview (getUserMedia in the main WKWebView, which has real
  // camera access once macOS Camera permission is granted). On any failure —
  // permission denied, no camera, API unsupported — falls back to the same
  // quiet "Camera preview unavailable" placeholder as before, plus a one-line
  // muted reason; never throws into the page. Tracks are stopped on destroy
  // (and before every re-acquire) so the camera light never stays on after
  // leaving Settings.
  let previewVideo = $state<HTMLVideoElement | null>(null);
  let previewStream = $state<MediaStream | null>(null);
  let previewError = $state<string | null>(null);
  let acquiredCameraId = $state<string | null>(null);
  let previewRequestId = 0;

  // Live macOS camera TCC status (issue #8). Seeded by acquirePreview's
  // ensureCameraAccess gate; null until the first check resolves. Drives both
  // the preview box's denied-recovery UI and the Camera PermissionRow below,
  // overriding the static `cameraStatus` prop once known. The gate only runs
  // with a real Tauri backend — in a plain browser the permission wrappers
  // all fall back to 'not-determined', which would wrongly override a
  // harness-supplied denied `cameraStatus`, so there the prop drives.
  const hasTauri = hasTauriBridge();
  let cameraAuth = $state<AuthStatus | null>(null);
  const cameraDenied = $derived(
    cameraAuth === null
      ? cameraStatus === 'denied'
      : cameraAuth === 'denied' || cameraAuth === 'restricted'
  );
  // Camera is REQUIRED, not optional (issue #25, user decision) — an
  // undecided TCC status renders as 'up-next' (needs action), never as the
  // dim skippable 'optional' row.
  const cameraRowStatus = $derived<PermissionStatus>(
    cameraAuth === null
      ? cameraStatus
      : cameraAuth === 'authorized'
        ? 'enabled'
        : cameraDenied
          ? 'denied'
          : 'up-next'
  );

  $effect(() => {
    // Load the real mic/speaker device lists once (issue #28). Only with
    // a real Tauri backend — the plain-browser harness keeps sample options.
    if (!hasTauriBridge || audioDevicesLoaded) return;
    void (async () => {
      try {
        const lists = await listAudioDevices();
        if (lists) {
          realMics = lists.recording.map((d) => ({ id: d.id, label: d.name }));
          realSpeakers = lists.playout.map((d) => ({ id: d.id, label: d.name }));
        }
      } catch (e) {
        audioDevicesError = String(e);
      } finally {
        audioDevicesLoaded = true;
      }
    })();
  });

  $effect(() => {
    if (!hasTauri || cameraDevicesLoaded) return;
    void (async () => {
      try {
        const devices = await listCameraDevices();
        if (devices) nativeCameras = devices.map((d) => ({ id: d.id, label: d.name }));
        void refreshCameraModes();
      } catch (e) {
        cameraDevicesError = String(e);
      } finally {
        cameraDevicesLoaded = true;
      }
    })();
  });

  function releaseStream(stream: MediaStream | null) {
    stream?.getTracks().forEach((t) => t.stop());
  }

  function clearPreviewStream() {
    if (previewVideo) previewVideo.srcObject = null;
    releaseStream(previewStream);
    previewStream = null;
  }

  function stopPreview() {
    previewRequestId += 1;
    clearPreviewStream();
  }

  async function acquirePreview(deviceId: string) {
    stopPreview();
    const requestId = previewRequestId;
    acquiredCameraId = deviceId;
    previewError = null;
    // Gate on the app-level TCC status BEFORE touching getUserMedia
    // (issue #8): 'not-determined' triggers the real OS prompt inside
    // ensureCameraAccess; 'denied'/'restricted' short-circuits to the
    // recovery UI — getUserMedia would only throw NotAllowedError instantly.
    let auth: AuthStatus | null = null;
    if (hasTauri) {
      auth = await ensureCameraAccess();
      cameraAuth = auth;
      if (auth === 'denied' || auth === 'restricted') {
        previewError = 'camera permission denied';
        return;
      }
    } else if (cameraStatus === 'denied') {
      // Plain-browser harness declaring a denied state: keep the recovery UI
      // (cameraDenied derives from the prop here), skip getUserMedia.
      previewError = 'camera permission denied';
      return;
    }
    try {
      if (!navigator.mediaDevices?.getUserMedia) {
        throw new DOMException('unsupported', 'NotSupportedError');
      }
      const selectedCameraId = realCameras.some((c) => c.id === deviceId) ? deviceId : undefined;
      const constraints: MediaStreamConstraints = {
        video: cameraPreviewConstraints(selectedCameraId)
      };
      let timedOut = false;
      let timeoutId: ReturnType<typeof setTimeout> | null = null;
      const cameraRequest = navigator.mediaDevices.getUserMedia(constraints);
      cameraRequest.then((lateStream) => {
        if (timedOut || requestId !== previewRequestId) releaseStream(lateStream);
      }, () => {});
      let stream: MediaStream;
      try {
        stream = await Promise.race([
          cameraRequest,
          new Promise<never>((_, reject) => {
            timeoutId = setTimeout(() => {
              timedOut = true;
              reject(new DOMException('camera request timed out', 'TimeoutError'));
            }, 10000);
          })
        ]);
      } finally {
        if (timeoutId) clearTimeout(timeoutId);
      }
      if (requestId !== previewRequestId) {
        releaseStream(stream);
        return;
      }
      previewStream = stream;
      if (previewVideo) {
        previewVideo.srcObject = stream;
        void previewVideo.play().catch(() => {});
      }
      // Enumerate only after a granted stream — labels/deviceIds are empty
      // before permission is granted.
      const devices = await navigator.mediaDevices.enumerateDevices();
      if (requestId !== previewRequestId) return;
      realCameras = devices
        .filter((d) => d.kind === 'videoinput')
        .map((d, i) => ({ id: d.deviceId, label: d.label || `Camera ${i + 1}` }));
    } catch (err) {
      if (requestId !== previewRequestId) return;
      clearPreviewStream();
      const name = err instanceof DOMException ? err.name : '';
      if (name === 'NotAllowedError' && auth === 'authorized') {
        // App-level TCC says authorized but the webview still refused —
        // that points at a WebKit-helper-process attribution problem
        // (issue #8 step 3's hypothesis). Log loudly so it's diagnosable
        // from the console/petal.log rather than looking like plain denial.
        console.error(
          'Settings camera preview: TCC reports authorized but getUserMedia threw NotAllowedError — possible WebKit GPU-helper TCC attribution issue'
        );
      }
      previewError =
        name === 'NotAllowedError'
          ? 'camera permission denied'
          : name === 'NotFoundError' || name === 'OverconstrainedError'
            ? 'no camera found'
            : name === 'NotReadableError'
              ? 'camera is in use by another app'
              : name === 'TimeoutError'
                ? 'camera request timed out'
            : 'camera unavailable';
    }
  }

  $effect(() => {
    // Initial mount only. Device switches call `handleCameraSelect` directly;
    // keeping acquisition out of broad reactive dependencies avoids the
    // previewStream/srcObject feedback loop that made the preview thrash (#170).
    if (acquiredCameraId === null) void acquirePreview(cameraValue);
  });

  $effect(() => {
    const el = previewVideo;
    if (!el) return;
    el.srcObject = previewStream;
    if (previewStream) void el.play().catch(() => {});
  });

  $effect(() => {
    return stopPreview;
  });

  $effect(() => {
    if (!hasTauri || aiChatLoaded) return;
    void (async () => {
      try {
        aiChat = await invoke<AiChatSettings>(COMMANDS.aiChatSettings);
      } catch (e) {
        aiChatError = `Could not load AI chat settings: ${e}`;
      } finally {
        aiChatLoaded = true;
      }
    })();
  });

  $effect(() => {
    if (!hasTauri || debugSettingsLoaded) return;
    void (async () => {
      try {
        debugSettings = await invoke<DebugModeSettings>(COMMANDS.debugModeSettings);
      } catch (e) {
        debugModeError = `Could not load the debug mode setting: ${e}`;
      } finally {
        debugSettingsLoaded = true;
      }
    })();
  });

  $effect(() => {
    if (!hasTauri || buildInfo) return;
    void invoke<BuildInfo>(COMMANDS.getBuildInfo).then(
      (info) => (buildInfo = info),
      () => {}
    );
  });

  $effect(() => {
    if (!showTestCockpit || !hasTauri) return;
    void refreshCockpitStatus();
    void refreshCockpitRuns();
    let unlisten: UnlistenFn | null = null;
    let cancelled = false;
    listen<TestProgressEvent>(EVENTS.testProgress, (event) => {
      cockpitProgress = [...cockpitProgress, event.payload];
      cockpitStatus = {
        running: event.payload.phase !== 'completed',
        runId: event.payload.runId,
        selector: event.payload.selector,
        resultsDir: event.payload.resultsDir,
        summary: event.payload.summary ?? cockpitStatus?.summary ?? null
      };
      // Reload the live run so already-finished scenarios' verdicts stream onto
      // their rows as later ones run; on completion also clear the busy button.
      if (event.payload.phase === 'scenario' || event.payload.phase === 'completed') {
        void refreshCockpitRuns(true);
      }
      if (event.payload.phase === 'completed') runningSelector = null;
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  });

  // ---- #923 layout: section index chips + AI chat consent step ---------------
  // The chip row mirrors the sections in DOM order (Permissions and the Test
  // cockpit are conditional in both places), so a chip resolves its section
  // by index — the Permissions section markup is pinned by tests and cannot
  // carry an id.
  const sectionChips = $derived.by(() => {
    const chips = [{ label: 'Devices' }];
    if (isMac()) chips.push({ label: 'Permissions' });
    chips.push({ label: 'Privacy' }, { label: 'AI chat' }, { label: 'Diagnostics' });
    if (showTestCockpit) chips.push({ label: 'Cockpit' });
    chips.push({ label: 'Account' }, { label: 'About' });
    return chips;
  });
  let settingsBody = $state<HTMLElement | null>(null);
  let currentSectionIndex = $state(0);
  // While a chip-initiated (smooth) scroll is in flight, the scroll-spy must
  // not fight it: the body's bottom clamp would otherwise re-highlight the
  // last chip when a short penultimate section can only reach max scroll.
  let jumpTargetTop: number | null = null;
  let jumpDeadline = 0;

  function bodySections(): HTMLElement[] {
    return settingsBody
      ? Array.from(settingsBody.querySelectorAll<HTMLElement>(':scope > section.section'))
      : [];
  }

  function jumpToSection(index: number) {
    const body = settingsBody;
    const target = bodySections()[index];
    if (!body || !target) return;
    const reduced = window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false;
    const top = Math.min(Math.max(0, target.offsetTop - 6), body.scrollHeight - body.clientHeight);
    jumpTargetTop = top;
    jumpDeadline = Date.now() + 800;
    body.scrollTo({ top, behavior: reduced ? 'auto' : 'smooth' });
    currentSectionIndex = index;
  }

  function handleBodyScroll() {
    const body = settingsBody;
    if (!body) return;
    if (jumpTargetTop !== null) {
      if (Math.abs(body.scrollTop - jumpTargetTop) > 1 && Date.now() < jumpDeadline) return;
      jumpTargetTop = null; // arrived (or gave up): the user's chip choice stands
      return;
    }
    const sections = bodySections();
    const probe = body.scrollTop + 56;
    let current = 0;
    sections.forEach((section, index) => {
      if (section.offsetTop <= probe) current = index;
    });
    // At the very bottom the last section may never reach the probe line.
    if (body.scrollTop + body.clientHeight >= body.scrollHeight - 2) current = sections.length - 1;
    currentSectionIndex = current;
  }

  // Turning AI chat ON is a consent boundary (see the section comment below):
  // the switch does not flip until the user confirms the two consequences.
  // Turning it OFF is immediate.
  let aiChatConsentOpen = $state(false);

  function handleAiChatSwitch(event: Event & { currentTarget: HTMLInputElement }) {
    const enabled = event.currentTarget.checked;
    if (enabled && !aiChat.enabled) {
      event.currentTarget.checked = false;
      aiChatConsentOpen = true;
      return;
    }
    aiChatConsentOpen = false;
    void handleAiChatEnabledChange(enabled);
  }

  function confirmAiChat() {
    aiChatConsentOpen = false;
    void handleAiChatEnabledChange(true);
  }
</script>

<!-- Escape closes the reset confirm from anywhere while it is open
     (the handler is a no-op when resetConfirmOpen is false). -->
<svelte:window onkeydown={handleResetKeydown} />
<div class="settings" class:frameless>
  <div class="settings-header" data-tauri-drag-region>
    <span class="title" data-tauri-drag-region>Settings</span>
  </div>

  <!-- #923: one chip per section, in DOM order. Jumps scroll the body; the
       current chip follows the scroll position. -->
  <nav class="section-index" aria-label="Settings sections">
    {#each sectionChips as chip, index (chip.label)}
      <button
        type="button"
        class="index-chip"
        aria-current={currentSectionIndex === index ? 'true' : undefined}
        onclick={() => jumpToSection(index)}
      >
        {chip.label}
      </button>
    {/each}
  </nav>

  <div class="settings-body" bind:this={settingsBody} onscroll={handleBodyScroll}>
    <!-- ============ Devices ============ -->
    <section class="section">
      <h2 class="section-title">Devices</h2>
      <div class="group">
        <div class="row stack">
          {#if cameraDenied}
            <!-- Camera TCC is denied/restricted (issue #8): show the real
                 recovery path — System Settings deep link (Privacy & Security →
                 Camera) + relaunch hint + retry — never the bare dead-end
                 placeholder. Interactive, so no aria-hidden here. -->
            <div class="preview-box denied">
              <span class="preview-label">Camera access is turned off for Petal</span>
              <span class="preview-reason">
                Enable Petal under Privacy &amp; Security → Camera, then relaunch if the preview stays dark.
              </span>
              <div class="preview-actions">
                <Button variant="primary" onclick={() => void openPrivacySettings('camera')}>
                  Open System Settings
                </Button>
                <Button variant="ghost" onclick={() => void acquirePreview(cameraValue)}>Try again</Button>
              </div>
            </div>
          {:else}
            <div class="preview-box" aria-hidden="true">
              {#if previewStream}
                <!-- Real camera feed via getUserMedia (see acquirePreview above). -->
                <!-- svelte-ignore a11y_media_has_caption -->
                <video class="preview-video" bind:this={previewVideo} autoplay playsinline muted></video>
              {:else}
                <!-- Fallback placeholder — shown until a stream is acquired, or
                     when acquisition fails (reason line below). -->
                <svg width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M2 7a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2z"></path>
                  <path d="M16 10l5-3v10l-5-3"></path>
                </svg>
                <span class="preview-label">Camera preview unavailable</span>
                {#if previewError}
                  <span class="preview-reason">{previewError}</span>
                {/if}
              {/if}
            </div>
          {/if}
          <span class="row-title">Camera</span>
          <DeviceSelect
            id="camera-device"
            label="Camera"
            value={cameraValue}
            options={cameraOptions}
            emptyLabel="No cameras found"
            disabled={noCameras || cameraDevicesError !== null}
            onchange={(value) => void handleCameraSelect(value)}
          />
          {#if cameraDevicesError}
            <span class="device-note error">{cameraDevicesError}</span>
          {:else if cameraNote}
            <span class:status-error={cameraNote.startsWith('Could not')} class="device-note">{cameraNote}</span>
          {/if}
        </div>

        {#if !isMac()}
          <div class="row stack">
            <span class="row-title">Camera resolution</span>
            <DeviceSelect
              id="camera-resolution"
              label="Camera resolution"
              value={cameraResolutionId}
              options={CAMERA_RESOLUTION_PRESETS.map((p) => ({ id: p.id, label: p.label }))}
              emptyLabel="No camera"
              disabled={!cameraModesAvailable}
              disabledOptions={cameraResolutionDisabled}
              onchange={(value) => void handleResolutionSelect(value)}
            />
          </div>

          <div class="row stack">
            <span class="row-title">Camera frame rate</span>
            <DeviceSelect
              id="camera-fps"
              label="Camera frame rate"
              value={String(cameraFps)}
              options={CAMERA_FPS_PRESETS.map((fps) => ({ id: String(fps), label: `${fps} fps` }))}
              emptyLabel="No camera"
              disabled={!cameraModesAvailable || cameraResolutionId === 'auto'}
              disabledOptions={cameraFpsDisabled}
              onchange={(value) => void handleFpsSelect(value)}
            />
            {#if hasDisabledCameraPresets && cameraModesAvailable}
              <span class="device-note">Greyed-out modes are not supported by this camera.</span>
            {/if}
            {#if cameraPrefsNote}
              <span class:status-error={cameraPrefsNote.startsWith('Could not')} class="device-note">{cameraPrefsNote}</span>
            {/if}
          </div>
        {/if}

        {#if audioDevicesError}
          <!-- Honest backend failure (no audio hardware / ADM init failed) —
               surfaced instead of silently keeping stale sample options. -->
          <div class="row stack">
            <span class="device-note error">{audioDevicesError}</span>
          </div>
        {/if}

        <div class="row stack">
          <span class="row-title">Microphone</span>
          <DeviceSelect
            id="microphone-device"
            label="Microphone"
            value={micValue}
            options={micOptions}
            emptyLabel="No microphones found"
            disabled={noMics || audioDevicesError !== null}
            onchange={(value) => void handleMicSelect(value)}
          />
          {#if micNote}
            <span class="device-note">{micNote}</span>
          {/if}
        </div>

        <div class="row stack">
          <span class="row-title">Speaker</span>
          <DeviceSelect
            id="speaker-device"
            label="Speaker"
            value={speakerValue}
            options={speakerOptions}
            emptyLabel="No speakers found"
            disabled={noSpeakers || audioDevicesError !== null}
            onchange={(value) => void handleSpeakerSelect(value)}
          />
          {#if speakerNote}
            <span class="device-note">{speakerNote}</span>
          {/if}
        </div>
      </div>
    </section>

    <!-- ============ Permissions (macOS-only: no TCC on Windows) ============ -->
    {#if isMac()}
    <section class="section">
      <h2 class="section-title">Permissions</h2>
      <div class="permission-list">
        <PermissionRow
          icon="screen"
          title="Screen Recording"
          required
          status={screenRecordingStatus}
          onOpenSettings={onOpenSettings ?? (() => void openPrivacySettings('screenRecording'))}
        />
        <PermissionRow
          icon="mic"
          title="Microphone"
          required
          status={micStatus}
          onOpenSettings={onOpenSettings ?? (() => void openPrivacySettings('microphone'))}
        />
        <!-- Camera row tracks the LIVE TCC status once the preview's gate has
             run (cameraRowStatus), and its denied-state recovery opens the
             real Camera pane — the shared onOpenSettings prop
             can't know which Privacy pane a row needs (issue #8). -->
        <PermissionRow
          icon="camera"
          title="Camera"
          required
          status={cameraRowStatus}
          onOpenSettings={() => void openPrivacySettings('camera')}
        />
        <PermissionRow
          icon="accessibility"
          title="Accessibility"
          required
          status={accessibilityStatus}
          onOpenSettings={() => void openPrivacySettings('accessibility')}
        />
      </div>
    </section>
    {/if}

    <!-- ============ Privacy &amp; Sharing ============ -->
    <section class="section">
      <h2 class="section-title">Privacy &amp; Sharing</h2>
      <div class="group">
        <!-- Remote-control policy (consent flow): three selectable rows in one
             radiogroup. Every label WRAPS (no nowrap anywhere) so the 400px
             main window can never clip the copy. -->
        <fieldset class="policy-row" aria-describedby="remote-control-policy-description">
          <legend class="row-title">{REMOTE_CONTROL_POLICY_TITLE}</legend>
          <span class="row-description" id="remote-control-policy-description">
            {REMOTE_CONTROL_POLICY_DESCRIPTION}
          </span>
          <div class="policy-options" role="radiogroup" aria-label={REMOTE_CONTROL_POLICY_TITLE}>
            {#each REMOTE_CONTROL_POLICY_OPTIONS as option (option.value)}
              <label class="policy-option" class:selected={remoteControlPolicy === option.value}>
                <input
                  type="radio"
                  name="remote-control-policy"
                  value={option.value}
                  checked={remoteControlPolicy === option.value}
                  onchange={() => onRemoteControlPolicyChange?.(option.value)}
                />
                <span class="policy-option-copy">
                  <span class="policy-option-label">{option.label}</span>
                  <span class="policy-option-hint">{option.hint}</span>
                </span>
                <svg class="policy-check" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                  <path d="M5 12.5 10 17.5 19 7"></path>
                </svg>
              </label>
            {/each}
          </div>
        </fieldset>
        <label class="row switch-row">
          <span class="row-copy">
            <span class="row-title">Local echo (experimental)</span>
            <span class="row-description">
              Instant local preview of your own clicks and typing while you control someone else's
              window — a prediction, not confirmation. Off by default.
            </span>
          </span>
          <Switch
            checked={localEchoEnabled}
            onchange={(e) => onLocalEchoEnabledChange?.(e.currentTarget.checked)}
          />
        </label>
        <label class="row switch-row">
          <span class="row-copy">
            <span class="row-title">{DEBUG_MODE_SETTING_TITLE}</span>
            <span class="row-description">{DEBUG_MODE_SETTING_DESCRIPTION}</span>
          </span>
          <Switch
            checked={debugSettings.enabled}
            onchange={(e) => void handleDebugModeEnabledChange(e.currentTarget.checked)}
          />
        </label>
        {#if debugModeError}
          <div class="row stack"><span class="device-note error">{debugModeError}</span></div>
        {/if}
      </div>
    </section>

    <!-- ============ AI chat (#656) ============
         ONE switch, OFF by default, and it is the consent boundary for the
         WHOLE feature: with it off, nothing about AI chat appears anywhere
         else in the app (the hover-tab entry and the remote-window header
         control are both absent, not disabled). Turning it on has two
         consequences and the copy states both — you get the control on every
         shared window, AND other participants can start a session on a window
         YOU share, which sends that window and the room's voice to Google. Do
         not compress these back into one line: the second is the one a user
         would not otherwise expect. #923 moved that copy into a confirm step
         that opens when the switch is flipped on (the switch stays off until
         the user confirms), and the key field into the section below the
         switch, shown once the feature is on or a key is already saved. -->
    <section class="section">
      <h2 class="section-title">AI chat</h2>
      <div class="group">
        <label class="row switch-row">
          <span class="row-copy">
            <span class="row-title">AI chat on shared windows</span>
            <span class="row-description">
              {#if aiChat.enabled}
                On — anyone in your meetings can start AI chat on a window you share.
              {:else}
                Adds an AI chat button to every shared window in your meetings.
              {/if}
            </span>
          </span>
          <Switch checked={aiChat.enabled} disabled={aiChatBusy} onchange={handleAiChatSwitch} />
        </label>
        {#if aiChatConsentOpen}
          <div class="consent" role="group" aria-label={AI_CHAT_CONSENT_TITLE}>
            <span class="row-description">{AI_CHAT_CONSENT_DESCRIPTION}</span>
            <span class="row-description consent-warning">
              {AI_CHAT_CONSENT_SHARED_WINDOW_WARNING}
            </span>
            <div class="consent-actions">
              <Button variant="ghost" onclick={() => (aiChatConsentOpen = false)}>Cancel</Button>
              <Button variant="primary" onclick={confirmAiChat}>{AI_CHAT_CONSENT_TITLE}</Button>
            </div>
          </div>
        {/if}
        {#if aiChat.enabled || aiChat.hasApiKey}
          <div class="row stack sub-row">
            <span class="row-copy">
              <span class="row-title">Gemini API key <span class="row-optional">optional</span></span>
              <span class="row-description">
                Bring your own key and AI chat bills to your own Google account. {AI_CHAT_COST_NOTE}
              </span>
            </span>
            {#if aiChat.hasApiKey}
              <div class="ai-key-saved">
                <span class="ai-key-state">Key saved</span>
                <button
                  type="button"
                  class="reset-button small"
                  disabled={aiChatBusy}
                  onclick={() => void handleAiChatRemoveKey()}
                >
                  Remove
                </button>
              </div>
            {/if}
            <div class="ai-key-row">
              <input
                class="input ai-key-input"
                type="password"
                autocomplete="off"
                spellcheck="false"
                placeholder={aiChat.hasApiKey ? 'Replace the saved key' : 'Paste your Gemini API key'}
                bind:value={aiChatKeyDraft}
              />
              <Button
                variant="primary"
                disabled={aiChatBusy || aiChatKeyDraft.trim().length === 0}
                onclick={() => void handleAiChatSaveKey()}
              >
                Save
              </Button>
            </div>
            <span class="row-description">
              {AI_CHAT_KEY_DATA_USE_NOTE}
              <button type="button" class="link-button" onclick={() => void openAiChatKeyPage()}>
                Get a key at aistudio.google.com/apikey
              </button>
            </span>
            {#if aiChatNote}
              <span class="device-note">{aiChatNote}</span>
            {/if}
            {#if aiChatError}
              <span class="device-note error">{aiChatError}</span>
            {/if}
          </div>
        {:else if aiChatNote || aiChatError}
          <div class="row stack">
            {#if aiChatNote}
              <span class="device-note">{aiChatNote}</span>
            {/if}
            {#if aiChatError}
              <span class="device-note error">{aiChatError}</span>
            {/if}
          </div>
        {/if}
      </div>
    </section>

    <!-- ============ Diagnostics ============ -->
    <section class="section">
      <h2 class="section-title">Diagnostics</h2>
      <div class="group">
        <div class="row stack">
          <span class="row-copy">
            <span class="row-title">Export logs</span>
            <span class="row-description">
              Reveals a zip of your logs to attach to a bug report. Nothing leaves this device.
            </span>
          </span>
          <div class="row-controls">
            <select
              class="range-select"
              bind:value={exportLogsDays}
              disabled={exportLogsBusy}
              aria-label="Log export date range"
            >
              <option value={2}>Last 2 days</option>
              <option value={7}>Last 7 days</option>
              <option value={0}>All logs</option>
            </select>
            <Button variant="primary" disabled={exportLogsBusy} onclick={() => void handleExportLogs()}>
              {exportLogsBusy ? 'Exporting...' : 'Export logs'}
            </Button>
          </div>
          {#if exportLogsNote}
            <span class="device-note">{exportLogsNote}</span>
          {/if}
          {#if exportLogsError}
            <span class="device-note error">{exportLogsError}</span>
          {/if}
        </div>
        {#if isMac()}
        <label class="row switch-row">
          <span class="row-copy">
            <span class="row-title">Send crash and error reports to Sentry</span>
            <span class="row-description">
              Helps us diagnose problems and improve Petal. Used for diagnostics generally, not just crashes.
            </span>
          </span>
          <Switch
            checked={sentryEnabled}
            onchange={(e) => onSentryEnabledChange?.(e.currentTarget.checked)}
          />
        </label>
        {/if}
      </div>
    </section>

    {#if showTestCockpit}
      <!-- ============ Test Cockpit ============ -->
      <section class="section">
        <h2 class="section-title">Test cockpit</h2>
        <div class="cockpit-panel">
          <div class="cockpit-toolbar">
            <div class="cockpit-presets" role="group" aria-label="Quick run presets">
              {#each COCKPIT_PRESETS as preset (preset.id)}
                <button
                  type="button"
                  class="preset-btn"
                  class:busy={runningSelector === preset.selector && cockpitStatus?.running}
                  title={preset.description}
                  disabled={cockpitBusy || cockpitStatus?.running}
                  onclick={() => runPreset(preset)}
                >
                  <span class="play-glyph" aria-hidden="true">▶</span>
                  <span class="preset-label">{preset.label}</span>
                </button>
              {/each}
            </div>
            <div class="cockpit-run-actions">
              {#if cockpitStatus?.running}
                <Button variant="ghost" disabled={cockpitBusy} onclick={() => void handleCancelCockpit()}>
                  Cancel
                </Button>
              {/if}
              <Button
                variant="ghost"
                disabled={!cockpitStatus?.resultsDir}
                onclick={() => void handleOpenCockpitResults()}
              >
                Open results folder
              </Button>
            </div>
          </div>

          <div class="feature-groups">
            {#each cockpitFeatureGroupsView as group (group.code)}
              {@const collapsed = collapsedFeatures.has(group.code)}
              <section class="feature-group">
                <header class="feature-head">
                  <button
                    type="button"
                    class="feature-toggle"
                    aria-expanded={!collapsed}
                    onclick={() => toggleFeature(group.code)}
                  >
                    <span class="chevron" class:collapsed aria-hidden="true">▾</span>
                    <span class="feature-code">{group.code}</span>
                    <span class="feature-name">{group.name}</span>
                    <span class="feature-count">{group.journeys.length}</span>
                  </button>
                  <button
                    type="button"
                    class="play-btn feature-play"
                    title={group.runnableCount > 0
                      ? `Run all of ${group.name}`
                      : 'no runnable journeys in this feature'}
                    aria-label={`Run feature ${group.name}`}
                    disabled={cockpitBusy || cockpitStatus?.running || group.runnableCount === 0}
                    onclick={() => runFeature(group)}
                  >
                    <span class="play-glyph" aria-hidden="true">▶</span>
                  </button>
                </header>
                {#if !collapsed}
                  <ul class="journey-list">
                    {#each group.journeys as journey (journey.id)}
                      {@const status = journeyStatusInfo(journey.status)}
                      {@const live = journeyLiveState(journey)}
                      {@const runnable = journeyIsRunnable(journey)}
                      <li class="journey-row" class:is-running={live === 'running'}>
                        <button
                          type="button"
                          class="play-btn journey-play"
                          title={runnable
                            ? `Run ${journey.id} · ${journey.title}`
                            : 'not yet implemented (gap)'}
                          aria-label={runnable
                            ? `Run ${journey.title}`
                            : `${journey.title} — not yet implemented`}
                          disabled={!runnable || cockpitBusy || cockpitStatus?.running}
                          onclick={() => runJourney(journey)}
                        >
                          <span class="play-glyph" aria-hidden="true">▶</span>
                        </button>
                        <div class="journey-body">
                          <div class="journey-head-row">
                            <span class="journey-title">{journey.title}</span>
                            <span
                              class={`status-marker status-${status.token}`}
                              title={`Coverage: ${status.label}`}
                            >{status.marker}</span>
                            {#if live}
                              <span class={`live-chip live-${live}`}>{JOURNEY_LIVE_LABELS[live]}</span>
                            {/if}
                          </div>
                          <div class="journey-tags">
                            <span class="tag id-tag">{journey.id}</span>
                            <span class="tag dir-tag">{directionLabel(journey.direction)}</span>
                            <span class={`tag pri-tag pri-${journey.priority.toLowerCase()}`}
                              >{journey.priority}</span
                            >
                            <span class="tag depth-tag">{depthLabel(journey.depth)}</span>
                          </div>
                        </div>
                      </li>
                    {/each}
                  </ul>
                {/if}
              </section>
            {/each}
          </div>
          {#if cockpitMessage}
            <span class="device-note">{cockpitMessage}</span>
          {/if}
          {#if cockpitSummary}
            <span class="device-note">{cockpitSummary}</span>
          {/if}
          {#if cockpitResultsNote}
            <span class="device-note">{cockpitResultsNote}</span>
          {/if}
          {#if cockpitError}
            <span class="device-note error">{cockpitError}</span>
          {/if}
          {#if cockpitProgress.length > 0}
            <ol class="cockpit-progress">
              {#each cockpitProgress as item}
                <li>
                  <span>{item.phase}</span>
                  <span>{item.completed}/{item.total}</span>
                  <small>{item.message}</small>
                </li>
              {/each}
            </ol>
          {/if}
          {#if cockpitStatus?.summary?.skipped?.length}
            <div class="cockpit-skipped">
              <span class="row-title">Skipped</span>
              {#each cockpitStatus.summary.skipped as skipped}
                <span class="device-note">{skipped.id}: {skipped.reason}</span>
              {/each}
            </div>
          {/if}
          <TestCockpitResults
            runs={cockpitRuns}
            selectedRun={selectedCockpitRunView}
            loading={cockpitRunsLoading}
            error={cockpitRunsError}
            onRefresh={() => void refreshCockpitRuns(true)}
            onSelectRun={(runId) => void handleSelectCockpitRun(runId)}
            onOpenFolder={(path) => void handleOpenCockpitResults(path)}
          />
        </div>
      </section>
    {/if}

    <!-- ============ Account ============ -->
    <section class="section">
      <h2 class="section-title">Account</h2>
      <IdentitySetup
        bind:name={userName}
        bind:identity
        {onNameChange}
        {onIdentityChange}
      />
    </section>

    <!-- ============ About: updates + reset ============ -->
    <section class="section">
      <div class="section-head">
        <h2 class="section-title">About</h2>
        {#if buildInfo}
          <span class="section-meta">v{displayBuildVersion(buildInfo)} · {buildInfo.commit}</span>
        {/if}
      </div>
      <div class="group">
        <div class="row">
          <span class="row-copy">
            <span class="row-title">Updates</span>
            <span class="row-description">Petal checks automatically on launch.</span>
            {#if checkUpdatesNote}
              <span class="device-note">{checkUpdatesNote}</span>
            {/if}
            {#if checkUpdatesError}
              <span class="device-note error">{checkUpdatesError}</span>
            {/if}
          </span>
          <Button variant="ghost" disabled={checkUpdatesBusy} onclick={() => void handleCheckForUpdates()}>
            {checkUpdatesBusy ? 'Checking...' : 'Check for updates'}
          </Button>
        </div>
        <div class="row reset-row">
          <span class="row-copy">
            <span class="row-title">Reset Petal</span>
            <span class="row-description">
              Clears Petal's identity, rooms, favorites, device choices, and saved window positions. Petal will quit.
            </span>
            {#if resetNote}
              <span class="device-note">{resetNote}</span>
            {/if}
          </span>
          <div
            class="reset-actions"
            bind:this={resetActionsRoot}
            onfocusout={handleResetFocusOut}
          >
            <button
              type="button"
              class="reset-button danger"
              disabled={resetBusy}
              aria-haspopup="dialog"
              aria-expanded={resetConfirmOpen}
              onclick={toggleResetConfirm}
            >
              Reset…
            </button>
            <div
              class="reset-popover"
              class:open={resetConfirmOpen}
              class:open-above={resetOpenAbove}
              role="dialog"
              aria-label="Confirm reset"
              aria-hidden={!resetConfirmOpen}
            >
                <span class="reset-popover-note">
                  "Reset and quit" clears Petal's local data and quits.
                </span>
                {#if isMac()}
                  <span class="device-note">
                    To also reset macOS permissions, paste these commands into Terminal afterwards
                    (copied to your clipboard when you click "Reset and quit" below, or copy them now):
                  </span>
                  <div class="reset-command-row">
                    <pre class="reset-command">{permissionResetCommand}</pre>
                    <button type="button" class="reset-button copy" onclick={() => void copyPermissionResetCommand()}>
                      Copy
                    </button>
                  </div>
                {/if}
                {#if resetCopyFailedConfirm}
                  <span class="device-note error">
                    Could not copy the commands above automatically. Select and copy them manually
                    before continuing, since Petal won't be able to do it after it quits.
                  </span>
                {/if}
                {#if resetError}
                  <span class="device-note error">{resetError}</span>
                {/if}
                <div class="reset-popover-actions">
                  {#if resetCopyFailedConfirm}
                    <button type="button" class="reset-button confirm" disabled={resetBusy} onclick={() => void handleFactoryReset(true)}>
                      {resetBusy ? 'Resetting...' : 'Quit anyway'}
                    </button>
                  {:else}
                    <button type="button" class="reset-button confirm" disabled={resetBusy} onclick={() => void handleFactoryReset()}>
                      {resetBusy ? 'Resetting...' : 'Reset and quit'}
                    </button>
                  {/if}
                  <button type="button" class="reset-button" disabled={resetBusy} onclick={() => (resetConfirmOpen = false)}>
                    Cancel
                  </button>
                </div>
              </div>
          </div>
        </div>
      </div>
    </section>
  </div>
</div>

<style>
  /* ---- #923 layout: one row primitive, two heading styles, a section index ---- */
  .settings {
    display: flex;
    flex-direction: column;
    width: 380px;
    max-height: 640px;
    border-radius: var(--radius-menu);
    overflow: hidden;
    overscroll-behavior: none;
    background: var(--bg-base-2);
    border: 1px solid var(--fill-strong);
  }

  /* Frameless: the panel IS the window — no card frame, fills the route.
     The body keeps its own padding + internal scroll. */
  .settings.frameless {
    width: 100%;
    flex: 1;
    min-height: 0;
    max-height: none;
    border-radius: 0;
    border: none;
  }

  .settings-header {
    display: flex;
    align-items: center;
    height: 50px;
    padding: 0 18px;
    border-bottom: 1px solid var(--hairline);
    flex-shrink: 0;
  }

  .title {
    font: 700 14px var(--font-display);
    color: var(--text-primary);
    text-wrap: balance;
  }

  /* Section index: one chip per section. Seven chips do not fit one line at
     the 400px window width, so the row wraps (two lines) rather than hiding
     chips behind a sideways scroll. */
  .section-index {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    flex-shrink: 0;
    padding: 10px 14px;
    border-bottom: 1px solid var(--hairline);
  }

  .index-chip {
    flex-shrink: 0;
    padding: 6px 11px;
    border: 0;
    border-radius: var(--radius-pill);
    background: var(--fill-weak);
    color: var(--text-dim);
    font: 600 12px var(--font-ui);
    white-space: nowrap;
    cursor: pointer;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard);
  }

  .index-chip:hover {
    background: var(--fill-base);
    color: var(--text-strong);
  }

  .index-chip[aria-current='true'] {
    background: var(--fill-strong);
    color: var(--text-primary);
  }

  .index-chip:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  .settings-body {
    position: relative;
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    overscroll-behavior: none;
    padding: 8px 14px 22px;
    display: flex;
    flex-direction: column;
    gap: 20px;
  }

  .section {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .section-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 10px;
  }

  /* Heading style 1 of 2: the section label. */
  .section-title {
    margin: 0;
    padding: 8px 4px 2px;
    font: 700 11.5px var(--font-display);
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--text-muted);
    text-wrap: balance;
  }

  .section-meta {
    font: 500 11px var(--font-mono);
    color: var(--text-faint);
    font-variant-numeric: tabular-nums;
  }

  /* The group: one bordered surface per section; rows separate with a hairline. */
  .group {
    display: flex;
    flex-direction: column;
    border-radius: var(--radius-popover);
    background: var(--surface);
    border: 1px solid var(--hairline);
    overflow: visible; /* the reset popover overlays past the group edge */
  }

  .row {
    display: flex;
    align-items: center;
    gap: 14px;
    min-height: 48px;
    padding: 11px 14px;
    box-sizing: border-box;
  }

  .group > :last-child {
    border-bottom-left-radius: inherit;
    border-bottom-right-radius: inherit;
  }

  .row + .row,
  .policy-row + .row,
  .row + .policy-row,
  .consent + .row {
    border-top: 1px solid var(--hairline);
  }

  /* A row whose control needs the full width stacks: copy, then control. */
  .row.stack {
    flex-direction: column;
    align-items: stretch;
    gap: 8px;
  }

  .row.sub-row {
    background: var(--fill-weak);
  }

  .switch-row {
    cursor: pointer;
  }

  .row-copy {
    display: flex;
    min-width: 0;
    flex: 1;
    flex-direction: column;
    gap: 3px;
  }

  /* Heading style 2 of 2: the row title. */
  .row-title {
    font: 600 13.5px var(--font-ui);
    color: var(--text-primary);
    text-wrap: pretty;
  }

  .row-optional {
    margin-left: 4px;
    font: 500 11.5px var(--font-ui);
    color: var(--text-faint);
  }

  .row-description {
    font: 500 12px/1.4 var(--font-ui);
    color: var(--text-dim);
    text-wrap: pretty;
  }

  /* The half of the AI chat consent a user would not expect: what turning the
     switch on lets OTHER people do to a window of theirs. Lifted out of the
     muted ramp so it cannot read as fine print. Wraps like every other
     description — it is never clipped at any panel width. */
  .consent-warning {
    color: var(--warning);
    font-weight: 600;
  }

  .row-controls {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .row-controls :global(.btn) {
    flex-shrink: 0;
  }

  .row > :global(.btn) {
    flex-shrink: 0;
  }

  /* The consent step for AI chat: opens under the switch row, closes on
     Cancel or confirm. Same warning tint as the copy it carries. */
  .consent {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 12px 14px 14px;
    border-top: 1px solid var(--hairline);
    background: color-mix(in srgb, var(--warning) 7%, transparent);
  }

  .consent-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 2px;
  }

  /* Remote-control policy: a fieldset that reads as three selectable rows.
     Every label WRAPS (overflow-wrap, never nowrap) so the 400px main window
     can never clip the copy. */
  .policy-row {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 3px;
    margin: 0;
    padding: 11px 14px 8px;
    border: 0;
  }

  .policy-row legend {
    padding: 0;
  }

  .policy-options {
    display: flex;
    flex-direction: column;
    gap: 2px;
    margin: 8px -6px 0;
  }

  .policy-option {
    position: relative;
    display: flex;
    align-items: center;
    gap: 10px;
    min-width: 0;
    padding: 7px 8px 7px 6px;
    border-radius: var(--radius-input);
    cursor: pointer;
    transition: background-color var(--motion-fast) var(--ease-standard);
  }

  .policy-option:hover {
    background: var(--fill-weak);
  }

  .policy-option.selected {
    background: var(--fill-base);
  }

  /* The native radio stays the interactive element (keyboard, screen readers);
     the check glyph on the right is the visible state. */
  .policy-option input {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    margin: 0;
    opacity: 0;
    cursor: pointer;
  }

  .policy-option:has(input:focus-visible) {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  .policy-option-copy {
    display: flex;
    min-width: 0;
    flex: 1;
    flex-direction: column;
    gap: 1px;
  }

  .policy-option-label {
    font: 500 13px var(--font-ui);
    color: var(--text-primary);
    overflow-wrap: anywhere;
  }

  .policy-option-hint {
    font: 400 12px var(--font-ui);
    color: var(--text-muted);
    overflow-wrap: anywhere;
  }

  .policy-check {
    flex-shrink: 0;
    width: 16px;
    height: 16px;
    color: var(--text-strong);
    opacity: 0;
    transition: opacity var(--motion-fast) var(--ease-standard);
  }

  .policy-option.selected .policy-check {
    opacity: 1;
  }

  .preview-box {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 8px;
    height: 120px;
    border-radius: var(--radius-input);
    background: linear-gradient(160deg, var(--surface-2), var(--surface));
    box-shadow: var(--shadow-inset-hairline);
    color: var(--text-faint);
    overflow: hidden;
  }

  /* Denied-recovery variant (issue #8): interactive content, so let it
     grow past the fixed placeholder height and read at full opacity. */
  .preview-box.denied {
    height: auto;
    min-height: 120px;
    padding: 16px;
    box-sizing: border-box;
    color: var(--text-soft);
    text-align: center;
  }

  .preview-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 4px;
  }

  .preview-actions :global(.btn) {
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      opacity var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .preview-actions :global(.btn:active:not(:disabled)) {
    transform: scale(var(--press-scale, 0.96));
  }

  .preview-video {
    /* Real feed fills the existing placeholder box — same dims/radius/chrome. */
    width: 100%;
    height: 100%;
    object-fit: cover;
  }

  .preview-label {
    font: 500 11px var(--font-ui);
    text-wrap: balance;
  }

  .preview-reason {
    /* One-line muted failure reason under the placeholder label. */
    font: 500 10px var(--font-mono);
    color: var(--text-faint);
    text-wrap: pretty;
  }

  .input {
    height: 36px;
    border-radius: var(--radius-input);
    background: var(--fill-base);
    border: 1px solid var(--hairline-strong);
    padding: 0 10px;
    font: 500 13px var(--font-ui);
    color: var(--text-primary);
    box-sizing: border-box;
    min-width: 0;
  }

  .input::placeholder {
    color: var(--text-faint);
    font-size: 11.5px;
  }

  .input:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  /* One-line honest device status under a select (issue #28). */
  .device-note {
    font: 500 10.5px var(--font-mono);
    color: var(--text-faint);
    text-wrap: pretty;
  }

  .device-note.error {
    color: var(--danger);
  }

  /* AI chat key field (#656). The row must shrink the INPUT, never the button
     or its label — `min-width: 0` on the input is what lets the flex row do
     that at the real 400px window width instead of overflowing. */
  .ai-key-row {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .ai-key-input {
    flex: 1 1 auto;
    min-width: 0;
  }

  /* Button labels are the thing that must never shrink or clip. */
  .ai-key-row :global(.btn) {
    flex-shrink: 0;
  }

  .ai-key-saved {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
  }

  .ai-key-state {
    font: 600 12px var(--font-ui);
    color: var(--live-bright);
  }

  /* Text link in the settings register: wraps rather than clipping, since the
     URL is long relative to the 400px window. */
  .link-button {
    padding: 0;
    border: none;
    background: none;
    color: var(--id-blue);
    font: 500 12px/1.4 var(--font-ui);
    text-align: left;
    text-decoration: underline;
    text-underline-offset: 2px;
    overflow-wrap: anywhere;
    cursor: pointer;
  }

  .link-button:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  .permission-list {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .range-select {
    font: 500 12.5px var(--font-ui);
    color: var(--text-primary);
    background: var(--fill-base);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-input);
    padding: 7px 10px;
    min-width: 0;
  }

  .range-select:disabled {
    opacity: var(--disabled-opacity);
  }

  .range-select:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  /* The cockpit section keeps its own dense panel (privileged builds only). */
  .cockpit-panel {
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 12px;
    border-radius: var(--radius-popover);
    background: var(--surface);
    border: 1px solid var(--hairline);
  }

  .cockpit-toolbar {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .cockpit-presets {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }

  .preset-btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
    height: 30px;
    padding: 0 10px;
    border: 1px solid color-mix(in srgb, var(--id-blue) 30%, transparent);
    border-radius: var(--radius-chip);
    background: color-mix(in srgb, var(--id-blue) 12%, transparent);
    color: var(--text-primary);
    font: 700 11px var(--font-ui);
    cursor: pointer;
    transition:
      background var(--motion-fast) var(--ease-standard),
      opacity var(--motion-fast) var(--ease-standard);
  }

  .preset-btn:hover:not(:disabled) {
    background: color-mix(in srgb, var(--id-blue) 20%, transparent);
  }

  .preset-btn:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .preset-btn:disabled {
    cursor: default;
    opacity: 0.45;
  }

  .preset-btn .play-glyph {
    font-size: 9px;
    color: var(--id-blue);
  }

  .cockpit-run-actions {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px;
  }

  .cockpit-run-actions :global(.btn) {
    height: 34px;
    padding: 0 12px;
    font-size: 12px;
  }

  /* --- Feature-grouped journey list --- */
  .feature-groups {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .feature-group {
    display: flex;
    flex-direction: column;
    border-radius: var(--radius-input);
    background: var(--fill-weak);
    box-shadow: var(--shadow-inset-hairline);
    overflow: hidden;
  }

  .feature-head {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 6px 8px 6px 6px;
  }

  .feature-toggle {
    display: flex;
    flex: 1;
    align-items: center;
    gap: 8px;
    min-width: 0;
    padding: 4px 4px;
    border: 0;
    border-radius: var(--radius-chip);
    background: transparent;
    color: inherit;
    text-align: left;
    cursor: pointer;
  }

  .feature-toggle:hover {
    background: var(--fill-weak);
  }

  .feature-toggle:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 1px;
  }

  .chevron {
    flex-shrink: 0;
    width: 12px;
    font-size: 10px;
    color: var(--text-faint);
    transition: transform var(--motion-fast) var(--ease-standard);
  }

  .chevron.collapsed {
    transform: rotate(-90deg);
  }

  .feature-code {
    flex-shrink: 0;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 18px;
    height: 18px;
    border-radius: var(--radius-badge);
    background: color-mix(in srgb, var(--id-blue) 18%, transparent);
    color: var(--id-blue);
    font: 800 10px var(--font-mono);
  }

  .feature-name {
    flex: 1;
    min-width: 0;
    font: 700 12px var(--font-ui);
    color: var(--text-primary);
  }

  .feature-count {
    flex-shrink: 0;
    font: 700 10px var(--font-mono);
    color: var(--text-faint);
    font-variant-numeric: tabular-nums;
  }

  .play-btn {
    display: inline-flex;
    flex-shrink: 0;
    align-items: center;
    justify-content: center;
    border: 1px solid color-mix(in srgb, var(--live-bright) 26%, transparent);
    border-radius: var(--radius-chip);
    background: color-mix(in srgb, var(--live-bright) 12%, transparent);
    color: var(--live-bright);
    cursor: pointer;
    transition:
      background var(--motion-fast) var(--ease-standard),
      opacity var(--motion-fast) var(--ease-standard);
  }

  .play-btn:hover:not(:disabled) {
    background: color-mix(in srgb, var(--live-bright) 22%, transparent);
  }

  .play-btn:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 1px;
  }

  .play-btn:disabled {
    cursor: default;
    border-color: var(--fill-strong);
    background: var(--fill-weak);
    color: var(--text-faint);
    opacity: 0.55;
  }

  .play-btn .play-glyph {
    font-size: 10px;
    line-height: 1;
  }

  .feature-play {
    width: 30px;
    height: 26px;
  }

  .journey-play {
    width: 26px;
    height: 26px;
  }

  .journey-list {
    display: flex;
    flex-direction: column;
    gap: 4px;
    margin: 0;
    padding: 0 6px 6px;
    list-style: none;
  }

  .journey-row {
    display: flex;
    align-items: flex-start;
    gap: 8px;
    padding: 7px 8px;
    border-radius: var(--radius-chip);
    background: var(--fill-weak);
  }

  .journey-row.is-running {
    background: color-mix(in srgb, var(--id-blue) 14%, transparent);
    box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--id-blue) 40%, transparent);
  }

  .journey-body {
    display: flex;
    min-width: 0;
    flex: 1;
    flex-direction: column;
    gap: 5px;
  }

  .journey-head-row {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 6px;
  }

  .journey-title {
    font: 650 12px var(--font-ui);
    color: var(--text-primary);
    text-wrap: pretty;
  }

  .status-marker {
    font-size: 11px;
    line-height: 1;
  }

  .live-chip {
    display: inline-flex;
    align-items: center;
    padding: 1px 7px;
    border-radius: var(--radius-chip);
    font: 800 9.5px var(--font-mono);
    text-transform: uppercase;
    letter-spacing: 0.03em;
    background: var(--fill-strong);
    color: var(--text-faint);
  }

  .live-queued {
    background: var(--fill-strong);
    color: var(--text-muted);
  }

  .live-running {
    background: color-mix(in srgb, var(--id-blue) 22%, transparent);
    color: var(--id-blue);
  }

  .live-passed {
    background: color-mix(in srgb, var(--live-bright) 18%, transparent);
    color: var(--live-bright);
  }

  .live-failed {
    background: var(--danger-tint-16);
    color: var(--danger);
  }

  .live-skipped {
    background: color-mix(in srgb, var(--warning) 18%, transparent);
    color: var(--warning);
  }

  .journey-tags {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 5px;
  }

  .tag {
    display: inline-flex;
    align-items: center;
    padding: 1px 6px;
    border-radius: var(--radius-badge);
    background: var(--fill-base);
    color: var(--text-faint);
    font: 700 9.5px var(--font-mono);
    white-space: nowrap;
  }

  .id-tag {
    color: var(--text-muted);
  }

  .dir-tag {
    color: var(--text-faint);
    text-transform: none;
  }

  .pri-tag.pri-p0 {
    background: var(--danger-tint-12);
    color: var(--danger);
  }

  .pri-tag.pri-p1 {
    background: color-mix(in srgb, var(--warning) 16%, transparent);
    color: var(--warning);
  }

  .pri-tag.pri-p2 {
    background: var(--fill-base);
    color: var(--text-muted);
  }

  .cockpit-progress {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .cockpit-progress li {
    display: grid;
    grid-template-columns: minmax(70px, 1fr) auto;
    gap: 2px 8px;
    padding: 8px;
    border-radius: var(--radius-chip);
    background: var(--fill-weak);
    font: 600 10px var(--font-mono);
    color: var(--text-muted);
  }

  .cockpit-progress small {
    grid-column: 1 / -1;
    font: 500 10px/1.35 var(--font-ui);
    color: var(--text-faint);
  }

  .cockpit-skipped {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }


  /* The Reset button centers against its copy like every other row button.
     The confirm lives in an anchored popover that OVERLAYS the page, so
     opening it never shifts the row or the surrounding layout — the button
     stays exactly where it is (Escape / focus-out / Cancel close it). */
  .reset-row {
    align-items: center;
  }

  .reset-actions {
    position: relative;
    display: flex;
    flex-shrink: 0;
  }

  /* Petal popover surface: the same gradient raised surface, hairline,
     radius, shadow, and 6px anchor gap as DeviceSelect/RosterPopover (and
     the same z-index layer, 41). Always mounted (visibility-hidden) so the
     open direction can be measured before reveal; entrance/exit use the
     semantic motion roles and a restrained tokenized rise (reduced motion
     zeroes both duration and distance). */
  .reset-popover {
    position: absolute;
    top: calc(100% + 6px);
    right: 0;
    z-index: 41;
    width: min(320px, calc(100vw - 40px));
    padding: 12px;
    box-sizing: border-box;
    display: flex;
    flex-direction: column;
    gap: 10px;
    background: var(--popover-bg);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-popover);
    box-shadow: var(--shadow-panel);
    opacity: 0;
    visibility: hidden;
    pointer-events: none;
    transform: translateY(var(--motion-distance));
    transition:
      opacity var(--motion-exit) var(--ease-exit),
      transform var(--motion-exit) var(--ease-exit),
      visibility 0s linear var(--motion-exit);
  }

  .reset-popover.open-above {
    top: auto;
    bottom: calc(100% + 6px);
    transform: translateY(calc(var(--motion-distance) * -1));
  }

  .reset-popover.open,
  .reset-popover.open.open-above {
    opacity: 1;
    visibility: visible;
    pointer-events: auto;
    transform: translateY(0);
    transition:
      opacity var(--motion-enter) var(--ease-standard),
      transform var(--motion-enter) var(--ease-standard),
      visibility 0s;
  }

  .reset-popover-note {
    font: 500 11px/1.35 var(--font-ui);
    color: var(--text-primary);
  }

  .reset-popover-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }

  .reset-button {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    min-width: 96px;
    height: 34px;
    padding: 0 12px;
    /* A button, not a tile: buttons use the 12px control radius. */
    border-radius: var(--radius-control);
    border: 1px solid var(--hairline-strong);
    background: var(--fill-base);
    color: var(--text-primary);
    font: 700 12px var(--font-display);
    cursor: pointer;
    transition:
      background var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      opacity var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .reset-button.small {
    min-width: 0;
    height: 30px;
  }

  .reset-button:hover:not(:disabled) {
    background: var(--fill-bright);
  }

  .reset-button:active:not(:disabled) {
    transform: scale(var(--press-scale, 0.96));
  }

  /* The one destructive-coloured control on the panel. */
  .reset-button.danger,
  .reset-button.confirm {
    background: var(--danger-tint-16);
    border-color: color-mix(in srgb, var(--danger) 28%, transparent);
    color: var(--danger);
  }

  .reset-button:disabled {
    cursor: default;
    opacity: 0.4;
  }

  .reset-command-row {
    display: flex;
    align-items: flex-start;
    gap: 8px;
    margin: 4px 0;
  }

  .reset-command {
    flex: 1;
    min-width: 0;
    margin: 0;
    padding: 8px;
    overflow-x: auto;
    border-radius: var(--radius-chip);
    background: rgba(0, 0, 0, 0.22);
    box-shadow: var(--shadow-inset-hairline);
    color: var(--text-primary);
    font: 500 10px/1.45 var(--font-mono);
    white-space: pre;
    /* #270 follow-up: the body-wide `user-select: none` (styles/app.css)
       made this text impossible to manually select/copy -- this is the one
       place in the app where copying displayed text is the whole point. */
    user-select: text;
    -webkit-user-select: text;
  }

  .reset-button.copy {
    flex-shrink: 0;
  }
</style>
