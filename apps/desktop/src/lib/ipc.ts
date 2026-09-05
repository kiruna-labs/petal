import { listen, type EventCallback, type UnlistenFn } from '@tauri-apps/api/event';
import type { ShareSessionError } from '$lib/data/shareErrors';

export const COMMANDS = {
  aiChatControlApprove: 'ai_chat_control_approve',
  aiChatControlReject: 'ai_chat_control_reject',
  aiChatControlResume: 'ai_chat_control_resume',
  aiChatControlStatus: 'ai_chat_control_status',
  aiChatIsActive: 'ai_chat_is_active',
  aiChatPanelDismiss: 'ai_chat_panel_dismiss',
  aiChatPanelPresent: 'ai_chat_panel_present',
  aiChatPttEnd: 'ai_chat_ptt_end',
  aiChatPttStart: 'ai_chat_ptt_start',
  aiChatRemoteSession: 'ai_chat_remote_session',
  aiChatRequestPttEnd: 'ai_chat_request_ptt_end',
  aiChatRequestPttStart: 'ai_chat_request_ptt_start',
  aiChatRequestSendText: 'ai_chat_request_send_text',
  aiChatRequestStart: 'ai_chat_request_start',
  aiChatRequestStop: 'ai_chat_request_stop',
  aiChatSendText: 'ai_chat_send_text',
  aiChatSetApiKey: 'ai_chat_set_api_key',
  aiChatSetEnabled: 'ai_chat_set_enabled',
  aiChatSettings: 'ai_chat_settings',
  aiChatStart: 'ai_chat_start',
  aiChatStop: 'ai_chat_stop',
  animateMainWindowResize: 'animate_main_window_resize',
  autotestJoinResult: 'autotest_join_result',
  captureWindowThumbnail: 'capture_window_thumbnail',
  closeRegionWindow: 'close_region_window',
  checkCompatibleUpdateAvailable: 'check_compatible_update_available',
  checkAccessibility: 'check_accessibility',
  checkCamera: 'check_camera',
  checkMicrophone: 'check_microphone',
  checkScreenRecording: 'check_screen_recording',
  cockpitStatus: 'cockpit_status',
  getTestCockpitArtifactDataUrl: 'get_test_cockpit_artifact_data_url',
  getTestCockpitRun: 'get_test_cockpit_run',
  compositorActivateWindow: 'compositor_activate_window',
  compositorRaiseWindowForClick: 'compositor_raise_window_for_click',
  /** #875: raise ALL of a participant's shared windows, restacked to match
   * the sharer's own z-order, restoring any the local viewer had hidden.
   * Native implementation lands in a parallel lane (macOS + Windows,
   * src-tauri/src/lib.rs); registering here lets the count-pill click wire
   * up now -- invoking an unregistered command just rejects at runtime. */
  compositorRaiseParticipantWindows: 'compositor_raise_participant_windows',
  compositorBeginResize: 'compositor_begin_resize',
  compositorFitToSource: 'compositor_fit_to_source',
  compositorHideWindow: 'compositor_hide_window',
  compositorListWindows: 'compositor_list_windows',
  compositorPopOut: 'compositor_pop_out',
  compositorResizeWindow: 'compositor_resize_window',
  /** #844: current open/closed state of the receiver-side AI-chat overlay --
   * asked once on mount (the "ask AND listen" shape `aiChatRemoteSession`
   * already uses), since a header that (re)mounts after the overlay was
   * toggled must not assume a hardcoded `false`. */
  compositorAiChatOverlayIsOpen: 'compositor_ai_chat_overlay_is_open',
  /** #844: show/hide the receiver-side AI-chat transcript/input overlay --
   * macOS-only, same as the other `compositor*` commands (no Windows
   * implementation of this overlay yet). Rust is the single source of truth
   * for open/closed state -- see `EVENTS.aiChatOverlayOpenChanged`. */
  compositorSetAiChatOverlayOpen: 'compositor_set_ai_chat_overlay_open',
  compositorSetDrawActive: 'compositor_set_draw_active',
  compositorStartDrag: 'compositor_start_drag',
  compositorToggleDebugPanel: 'compositor_toggle_debug_panel',
  compositorWindowDebugStats: 'compositor_window_debug_stats',
  createRoom: 'create_room',
  currentRoom: 'current_room',
  debugModeSettings: 'debug_mode_settings',
  drawSend: 'draw_send',
  downloadAndInstallCompatibleUpdate: 'download_and_install_compatible_update',
  exportLogs: 'export_logs',
  forgetRoom: 'forget_room',
  frontendReady: 'frontend_ready',
  prepareFeedbackDiagnostics: 'prepare_feedback_diagnostics',
  galleryBridgeConfig: 'gallery_bridge_config',
  getBuildInfo: 'get_build_info',
  getEventJournal: 'get_event_journal',
  getMenubarState: 'get_menubar_state',
  getNetworkSnapshot: 'get_network_snapshot',
  getSharePriority: 'get_share_priority',
  hideMenubarPopover: 'hide_menubar_popover',
  hoverTabPageMounted: 'hover_tab_page_mounted',
  hoverTabDrag: 'hover_tab_drag',
  setHoverTabMenuOpen: 'set_hover_tab_menu_open',
  joinRoom: 'join_room_command',
  leaveRoom: 'leave_room_command',
  listAudioDevices: 'list_audio_devices',
  listCameraDevices: 'list_camera_devices',
  listCameraModes: 'list_camera_modes',
  listRoomOccupancy: 'list_room_occupancy',
  listRooms: 'list_rooms',
  listShareableWindows: 'list_shareable_windows',
  listTestCockpitRuns: 'list_test_cockpit_runs',
  logWindowStack: 'log_window_stack_command',
  logUpdaterEvent: 'log_updater_event',
  nextSelfViewFrame: 'next_self_view_frame',
  openDevTelepointerWindow: 'open_dev_telepointer_window',
  openMainRoute: 'open_main_route',
  showMainWindow: 'show_main_window',
  openNetworkCockpitWindow: 'open_network_cockpit_window',
  openRegionWindow: 'open_region_window',
  regionPlacementActive: 'region_placement_active',
  regionShareState: 'region_share_state',
  syncRegionWindowFrame: 'sync_region_window_frame',
  regionViewOptionsState: 'region_view_options_state',
  setRegionSharePriority: 'set_region_share_priority',
  setRegionDrawActive: 'set_region_draw_active',
  regionAiChatStart: 'region_ai_chat_start',
  regionAiChatStop: 'region_ai_chat_stop',
  toggleRegionShare: 'toggle_region_share',
  openPrivacySettings: 'open_privacy_settings',
  openTestCockpitResultsFolder: 'open_test_cockpit_results_folder',
  openTestPatternWindow: 'open_test_pattern_window',
  openWindowPickerWindow: 'open_window_picker_window',
  toggleWindowPickerWindow: 'toggle_window_picker_window',
  quitApp: 'quit_app',
  recordVideoStreamState: 'record_video_stream_state',
  recordCameraReceiveHealth: 'record_camera_receive_health',
  remoteControlAllowed: 'remote_control_allowed',
  remoteControlAnswerConsent: 'remote_control_answer_consent',
  remoteControlAnswerEscalation: 'remote_control_answer_escalation',
  remoteControlPolicy: 'remote_control_policy',
  remoteControlRevoke: 'remote_control_revoke',
  remoteControlRequestTimedOut: 'remote_control_request_timed_out',
  remoteControlRequestEscalation: 'remote_control_request_escalation',
  remoteClipboardCopy: 'remote_clipboard_copy',
  remoteClipboardPaste: 'remote_clipboard_paste',
  remoteControlSend: 'remote_control_send',
  remoteControlSetActive: 'remote_control_set_active',
  renameRoom: 'rename_room',
  requestCamera: 'request_camera',
  requestAccessibility: 'request_accessibility',
  requestMicrophone: 'request_microphone',
  requestScreenRecording: 'request_screen_recording',
  resetLocalRooms: 'reset_local_rooms',
  resizeMenubarPopover: 'resize_menubar_popover',
  restartApp: 'restart_app',
  roomPresence: 'room_presence',
  runLaunchUpdateCheck: 'run_launch_update_check',
  setAudioDevices: 'set_audio_devices',
  setCameraDevice: 'set_camera_device',
  setCameraPrefs: 'set_camera_prefs',
  setCockpitOpen: 'set_cockpit_open',
  setDebugMode: 'set_debug_mode',
  setMainPillMode: 'set_main_pill_mode',
  setMicMuted: 'set_mic_muted',
  setRemoteControlAllowed: 'set_remote_control_allowed',
  setRemoteControlPolicy: 'set_remote_control_policy',
  setSentryEnabled: 'set_sentry_enabled',
  setSharePriority: 'set_share_priority',
  setHoverTabTooltip: 'set_hover_tab_tooltip',
  setShareResolution: 'set_share_resolution',
  shareNoticeDismiss: 'share_notice_dismiss',
  shareNoticePresent: 'share_notice_present',
  controlConsentDismiss: 'control_consent_dismiss',
  controlConsentPresent: 'control_consent_present',
  shareOverlaySetDrawActive: 'share_overlay_set_draw_active',
  shareOverlayDrawActive: 'share_overlay_draw_active',
  shareWindow: 'share_window',
  setShareControlMode: 'set_share_control_mode',
  setShareRemoteControlAllowed: 'set_share_remote_control_allowed',
  shareRemoteControlAllowed: 'share_remote_control_allowed',
  sharedWindowIds: 'shared_window_ids',
  startTestCockpit: 'start_test_cockpit',
  startCameraPublish: 'start_camera_publish_command',
  stopCameraPublish: 'stop_camera_publish_command',
  cameraPublishState: 'camera_publish_state',
  cancelTestCockpit: 'cancel_test_cockpit',
  toggleMenubarMic: 'toggle_menubar_mic',
  toggleWindowShare: 'toggle_window_share',
  updateShareBorderFrame: 'update_share_border_frame'
} as const;

export const EVENTS = {
  aiChatControlRequest: 'ai-chat-control-request',
  aiChatControlResolved: 'ai-chat-control-resolved',
  /**
   * A session running on ANOTHER participant's machine, as observed over
   * `petal.ai-chat`. This is the receiver's only window onto a session it does
   * not host — `EVENTS.aiChatState` is emitted by the host engine and so never
   * fires here. Also emitted with `active: false` when a remote session is
   * cleared (owner disconnected, or its heartbeat went stale), so a crashed
   * host can never leave a phantom "AI chat live" badge behind. Absence of any
   * event means "no session".
   */
  aiChatRemoteState: 'ai-chat-remote-state',
  /** A transcript delta from a session hosted on someone else's machine
   * (#664). See {@link AiChatRemoteTranscriptEvent}. */
  aiChatRemoteTranscript: 'ai-chat-remote-transcript',
  /**
   * #844: fires whenever `compositor_set_ai_chat_overlay_open` changes the
   * receiver-side AI-chat overlay's open/closed state. RemoteWindowHeader's
   * "AI chat live" badge derives its state from this (plus
   * `COMMANDS.compositorAiChatOverlayIsOpen` on mount) instead of keeping its
   * own optimistic local copy -- Rust is the single source of truth, so the
   * overlay's own Escape-to-close and a retired-window restore both stay in
   * sync with the badge automatically.
   */
  aiChatOverlayOpenChanged: 'ai-chat-overlay-open-changed',
  aiChatState: 'ai-chat-state',
  aiChatTranscript: 'ai-chat-transcript',
  /**
   * Frontend-emitted (webview -> webview), NOT a Rust event: the only entry in
   * this table without a native emitter. `ai_chat_start` returns its refusal to
   * the CALLER, and the caller is the hover-tab panel — a fixed 232x37px native
   * frame with no room for a message. The hover tab re-emits the refusal on the
   * Tauri bus so the main window (which owns the toast surfaces) can render it.
   */
  aiChatRefused: 'ai-chat-refused',
  autotestJoinResult: 'autotest-join-result',
  cameraPublishState: 'camera-publish-state',
  /**
   * Debug mode (#669) changed, from `set_debug_mode`. The belt half of
   * "ask AND listen" -- an already-open remote-window surface webview reads
   * `COMMANDS.debugModeSettings` once on mount, then updates live from this
   * event without needing to be reopened. The AI chat setter never grew this
   * equivalent (a documented gap); this setting does not repeat it.
   */
  debugModeChanged: 'debug-mode-changed',
  /**
   * Debounced Rust event (window_change_watcher.rs): the desktop's window set
   * changed (a window was created, closed, minimized, or restored). The
   * window picker listens and soft-refreshes so its grid follows the desktop
   * without the manual Refresh button. Payload is a pure trigger (`void`);
   * the watcher already invalidated the Rust list cache before emitting, so
   * the listener's refresh sees the CURRENT window set.
   */
  desktopWindowsChanged: 'desktop-windows-changed',
  hoverTabHide: 'hover-tab-hide',
  hoverTabUpdate: 'hover-tab-update',
  drawUpdate: 'draw-update',
  journalAppended: 'journal-appended',
  meetingRestorePillRequested: 'meeting-restore-pill-requested',
  micMuteChanged: 'mic-mute-changed',
  networkStats: 'network-stats',
  presenceUpdate: 'presence-update',
  regionPlacementSettled: 'region-placement-settled',
  regionPlacementReleased: 'region-placement-released',
  regionShareStateChanged: 'region-share-state-changed',
  regionViewOptionsChanged: 'region-view-options-changed',
  regionControlStateChanged: 'region-control-state-changed',
  remoteControlStatus: 'remote-control-status',
  /** Sharer-side consent prompt. The discriminated payload's `kind` is
   * `control` for a parked control request or `fullControlEscalation` for a
   * Windows mode escalation. Both are rendered by the same non-activating
   * control-consent panel and never auto-approve. */
  controlConsentRequested: 'control-consent-requested',
  /**
   * A REMOTE peer started sharing a window (#679) -- emitted from
   * `transport::subscriber` right after `compositor::ensure_window`
   * succeeds for a genuinely new share (never for own shares, a
   * republish/quality-switch, or a re-subscribe that follows a
   * transport-side teardown -- see
   * `compositor::consume_share_started_pill_suppression` on the Rust side).
   * The always-loaded, always-hidden `share-notice` route listens for this
   * to show the top-center "<Name> is sharing a window" pill.
   */
  remoteShareStarted: 'remote-share-started',
  regionWarning: 'region-warning',
  resilienceEvent: 'resilience-event',
  roomLeft: 'room-left',
  roomUpdated: 'room-updated',
  shareError: 'share-error',
  shareStateChanged: 'share-state-changed',
  shareControlModeChanged: 'share-control-mode-changed',
  sharePickerChanged: 'share-picker-changed',
  sharePickerOpened: 'share-picker-opened',
  sharePickerVisibilityChanged: 'share-picker-visibility-changed',
  telepointerUpdate: 'telepointer-update',
  testProgress: 'test-progress'
} as const;

export function hasTauriBridge(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}

export function listenUntilDestroy<T>(
  event: EventName,
  handler: EventCallback<T>,
  setUnlisten: (unlisten: UnlistenFn | undefined) => void,
  isDestroyed: () => boolean
): void {
  listen<T>(event, handler)
    .then((unlisten) => {
      if (isDestroyed()) {
        unlisten();
      } else {
        setUnlisten(unlisten);
      }
    })
    .catch(() => {});
}

export type CommandName = (typeof COMMANDS)[keyof typeof COMMANDS];
export type EventName = (typeof EVENTS)[keyof typeof EVENTS];

/**
 * AI chat settings as the frontend is allowed to see them. The user's Gemini
 * API key is deliberately NOT included — only whether one is configured — so a
 * webview can never read the key back out (mirrors `settings::Redacted`).
 */
export interface AiChatSettings {
  enabled: boolean;
  hasApiKey: boolean;
}

/**
 * Debug mode (#669): the master switch gating the remote-window header's
 * Debug button. Mirrors Rust's `debug_settings::DebugSettings`.
 */
export interface DebugModeSettings {
  enabled: boolean;
}

/**
 * Why an AI chat session ended or refused to start. Mirrors Rust's
 * `ai_chat::state::EndReason` and the `petal.ai-chat` wire vocabulary — a
 * closed set, so no surface can invent its own error string.
 */
export type AiChatEndReason =
  | 'stopped'
  | 'time-limit'
  | 'disabled'
  | 'not-shared'
  | 'busy'
  | 'rate-limited'
  | 'hosted-unavailable'
  | 'offline'
  | 'mint-failed'
  | 'model-unavailable'
  | 'quota'
  | 'error';

/** Result of asking to start a session. A refusal always names its reason. */
export interface AiChatStartOutcome {
  started: boolean;
  reason?: AiChatEndReason;
}

export interface AiChatPanelInfo {
  ownerAppName: string | null;
}

/** Lifecycle of a running session, as pushed on `EVENTS.aiChatState`. */
export type AiChatPhase =
  | { phase: 'connecting' }
  | { phase: 'live' }
  | { phase: 'ended'; reason: AiChatEndReason };

/**
 * `EVENTS.aiChatState` payload. The engine emits TWO shapes on this one event
 * (`ai_chat/session.rs`'s `emit_phase` / `emit_countdown`): a phase change
 * carries `state`, a countdown tick carries `secondsLeft`. Exactly one of the
 * two is present per emission — never both.
 */
export interface AiChatStateEvent {
  windowId: number;
  state?: AiChatPhase;
  secondsLeft?: number;
  /** Host-side echo of the room's authoritative PTT floor. */
  activeSpeaker?: string | null;
}

/**
 * `EVENTS.aiChatTranscript` payload. Deltas stream in; `final` closes the turn
 * (`TurnComplete` sends an assistant delta with EMPTY text and `final: true`,
 * so an empty final must close the open bubble rather than open a new one).
 */
export interface AiChatTranscriptEvent {
  windowId: number;
  role: 'user' | 'assistant';
  text: string;
  final: boolean;
}

/**
 * A session hosted by SOMEONE ELSE, as this client sees it over
 * `petal.ai-chat`. Mirrors Rust's `topic::RemoteState`, and is ONE shape for
 * both halves of ask-then-listen: a surface asks once on mount
 * (`COMMANDS.aiChatRemoteSession`, `null` when no session is known) and listens
 * thereafter (`EVENTS.aiChatRemoteState`), and must not have to reconcile two
 * vocabularies. Keep them identical — an earlier revision had the command
 * return a narrowed struct, which silently dropped a refusal that arrived
 * before the surface mounted.
 *
 * `windowId`/`ownerIdentity` are the routing key: the event is broadcast to
 * every surface, so each one must be able to tell whether it is theirs.
 *
 * `error` is the closed `AiChatEndReason` set, rendered through
 * `aiChatEndReasonMessage`. Note `busy` here means another participant holds
 * the push-to-talk floor, not that anything failed.
 */
export interface AiChatRemoteSessionState {
  windowId: number;
  ownerIdentity: string;
  active: boolean;
  startedBy?: string;
  secondsLeft?: number;
  activeSpeaker?: string;
  error?: AiChatEndReason;
}

/** `EVENTS.aiChatRemoteTranscript` payload (#664): a transcript delta from a
 * session hosted on someone else's machine, relayed as-is over
 * `petal.ai-chat` -- fold it with `appendTranscriptDelta`, the same
 * coalescing logic the LOCAL panel's own transcript already uses. */
export interface AiChatRemoteTranscriptEvent {
  windowId: number;
  ownerIdentity: string;
  role: 'user' | 'assistant';
  text: string;
  final: boolean;
}

/** `EVENTS.aiChatRefused` payload — see that event's note above. */
export interface AiChatRefusedEvent {
  windowId: number;
  reason: AiChatEndReason;
}

/** `EVENTS.aiChatOverlayOpenChanged` payload — see that event's note above. */
export interface AiChatOverlayOpenChangedEvent {
  windowId: number;
  ownerIdentity: string;
  open: boolean;
}

/**
 * What the model asked to do, as the approval card must render it (#658).
 * Mirrors Rust's `control_gate::ActionDetail`. `literalText` is the EXACT text
 * that would be typed and `element` the resolved control's role + title —
 * a card that only said "the AI wants to click something" would be consent
 * theatre, so both are rendered in full and never elided.
 */
export interface AiChatControlDetail {
  summary: string;
  literalText?: string;
  element?: string;
}

/**
 * `EVENTS.aiChatControlRequest` payload. `sessionId` + `requestId` together
 * identify what is being answered: BOTH are echoed back on approve/reject so a
 * click on a card the model had already replaced cannot authorize whatever
 * replaced it.
 */
export interface AiChatControlRequestEvent {
  windowId: number;
  tool: string;
  requestId: string;
  sessionId: number;
  detail: AiChatControlDetail;
}

/**
 * `EVENTS.aiChatControlResolved` payload. The card is dismissed only by one of
 * these — it ran, it was refused, or the session ended — never on a timer: a
 * control prompt that vanishes by itself trains people to ignore it.
 */
export interface AiChatControlResolvedEvent {
  windowId: number;
  requestId: string;
  ok: boolean;
  code: string;
}

/** Rust-authoritative standing window-control state for the live session. */
export interface AiChatControlStatus {
  sessionId: number;
  standing: 'ask' | 'session' | 'refused';
}

/** Env-gated debug-run join outcome. No normal user join emits this event. */
export type AutotestJoinResult =
  | { status: 'joined'; roomName: string }
  | { status: 'failed'; reason: 'permission_denied' | 'backend_token_unavailable' | 'room_connection_failed' | 'session_setup_failed' };

const RAW_WINDOW_TRACK_RE = /^petal-window(?:-\d+|-capture)?$/i;
const RAW_CAMERA_TRACK_RE = /^petal-camera(?:-.+)?$/i;

function isRawRemoteTrackName(label: string): boolean {
  return RAW_WINDOW_TRACK_RE.test(label) || RAW_CAMERA_TRACK_RE.test(label);
}

export function remoteWindowSourceLabel(sourceTitle: string): string {
  const trimmed = sourceTitle.trim();
  if (!trimmed || RAW_WINDOW_TRACK_RE.test(trimmed)) return 'Shared window';
  if (RAW_CAMERA_TRACK_RE.test(trimmed)) return 'Camera';
  const parts = trimmed.split(/\s+—\s*/).map((part) => part.trim());
  if (parts.length > 1) {
    const appName = parts[parts.length - 1];
    if (appName && !isRawRemoteTrackName(appName)) return appName;
    const windowTitle = parts.find((part) => part && !isRawRemoteTrackName(part));
    if (windowTitle) return windowTitle;
    if (parts.some((part) => RAW_CAMERA_TRACK_RE.test(part))) return 'Camera';
    if (parts.some((part) => RAW_WINDOW_TRACK_RE.test(part))) return 'Shared window';
  }
  return trimmed;
}

/**
 * Owner-name fallback shared by every remote-window surface: an empty/
 * whitespace-only name (e.g. a metadata race at share-start) reads as
 * "Someone" rather than a blank. Pulled out so the share-notice pill (#679)
 * reuses this exact fallback instead of a second bespoke one -- there is no
 * Rust equivalent of the web's `Guest`/`looksLikeTechnicalIdentity` sanitizer
 * (`web-harness/src/tiles.ts`'s `displayNameForParticipant`), so this stays
 * a plain trim + fallback, matching what the native side already sends.
 */
export function remoteWindowOwnerLabel(ownerName: string): string {
  return ownerName.trim() || 'Someone';
}

export function formatRemoteWindowHeaderTitle(sourceTitle: string, ownerName: string): string {
  return `${remoteWindowSourceLabel(sourceTitle)} by ${remoteWindowOwnerLabel(ownerName)}`;
}

/** Mirrors `BuildInfo` (src-tauri/src/lib.rs). */
export interface BuildInfo {
  version: string;
  commit: string;
  buildDate: string;
  isReleaseBuild: boolean;
  cockpitPrivileged: boolean;
  bundleIdentifier: string;
}

export type CockpitRunStatus = 'passed' | 'failed' | 'cancelled';

/**
 * A test-cockpit journey (the feature-first row model from the project history).
 * Authored there, mirrored into contracts/petal-contracts.json → testCockpitJourneys,
 * and consumed by the feature-grouped runner UI. Mirrors the contract fields exactly.
 */
export interface CockpitJourney {
  /** Stable id, e.g. "SHARE-03". */
  id: string;
  /** 1–3 word plain-language title, e.g. "Smooth & fast". */
  title: string;
  /** Feature code A–H. */
  feature: string;
  /** Peer direction: nat-nat | web-nat | nat-web | both | nat-local. */
  direction: string;
  /** Priority: P0 | P1 | P2. */
  priority: string;
  /** Depth: short | long | short-long. */
  depth: string;
  /** Coverage status: covered | partial | gap | blind-spot. */
  status: string;
  /** The concrete scenario the backend can drive, when one exists (else a gap). */
  runnable?: string;
  /** Legacy mechanics ids kept as hidden aliases so old selectors/results resolve. */
  legacy: string[];
}

export interface CockpitSkippedScenario {
  id: string;
  reason: string;
}

export interface CockpitSummary {
  status: CockpitRunStatus;
  passed: number;
  failed: number;
  skipped: CockpitSkippedScenario[];
  message: string;
}

export interface CockpitStatus {
  running: boolean;
  runId?: string | null;
  selector?: string | null;
  resultsDir?: string | null;
  summary?: CockpitSummary | null;
}

export interface TestCockpitRunSummary {
  runId: string;
  resultsDir: string;
  updatedAtUnixMs: number;
  status: 'passed' | 'failed' | 'skipped' | 'unknown';
  pass: number;
  fail: number;
  skipped: number;
  parseErrors: number;
}

export interface TestCockpitArtifact {
  type: string;
  path: string;
  stepId?: string | null;
  tMs?: number | null;
}

export interface TestCockpitEvent {
  kind: string;
  scenarioId?: string | null;
  payload: unknown;
}

export interface TestCockpitRunDetail {
  summary: TestCockpitRunSummary;
  events: TestCockpitEvent[];
  artifacts: TestCockpitArtifact[];
  scorecard?: unknown | null;
}

export interface TestProgressEvent {
  runId: string;
  selector: string;
  phase: string;
  scenarioId?: string | null;
  message: string;
  completed: number;
  total: number;
  skipped: CockpitSkippedScenario[];
  summary?: CockpitSummary | null;
  resultsDir?: string | null;
}

export interface WindowFrame {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** Mirrors `window_resize::AnimatedResizeOutcome` (src-tauri/src/window_resize.rs). */
export interface AnimatedResizeOutcome {
  applied: boolean;
  animated: boolean;
  width: number;
  height: number;
  reason: string | null;
}

/** Mirrors `window_source::ShareableWindow` (src-tauri/src/window_source.rs). */
export interface ShareableWindow {
  windowId: number;
  title: string | null;
  appName: string;
  appBundleId: string;
  appPid: number;
  appIconBase64: string | null;
  /** Windows only: 'display' entries are "Screen N" cards; omitted on macOS (defaults to 'window'). */
  kind?: 'window' | 'display';
}

/** Mirrors `rooms::RoomRecord` (src-tauri/src/rooms.rs). */
export interface RoomRecord {
  id: string;
  name: string;
  accessCode?: string | null;
  displayName?: string | null;
  slug: string;
  createdAtMs: number;
  lastJoinedMs?: number | null;
  open: boolean;
}

/** Mirrors `presence::PresentParticipant` (src-tauri/src/presence.rs). */
export interface PresentParticipant {
  identity: string;
  name: string;
  isLocal: boolean;
  speaking: boolean;
  micMuted: boolean;
}

export interface RoomOccupancyParticipant {
  identity: string;
  name: string;
}

/** Mirrors `rooms::RoomOccupancy` (src-tauri/src/rooms.rs). */
export interface RoomOccupancy {
  roomName?: string;
  name?: string;
  id?: string;
  slug?: string;
  open?: boolean;
  livekitRoom: string;
  available?: boolean;
  occupancy?: number;
  participants?: RoomOccupancyParticipant[];
  unavailableReason?: string;
}

/** Mirrors `compositor::RemoteWindowSummary` (src-tauri/src/compositor.rs). */
export interface RemoteWindowSummary {
  windowId: number;
  ownerIdentity: string;
  ownerDisplayName: string;
  sourceTitle: string;
  hidden: boolean;
}

/** Mirrors `presence::PresenceUpdate` (src-tauri/src/presence.rs). */
export interface PresenceUpdate {
  roomName: string;
  participants: PresentParticipant[];
}

/** Mirrors `session::RoomLeftEvent` (src-tauri/src/session.rs). */
export interface RoomLeftEvent {
  roomName: string;
}

/** Mirrors `menubar::MenubarPillState` (src-tauri/src/menubar.rs). */
export interface MenubarPillState {
  micMuted: boolean;
  cameraPublishing: boolean;
  inMeeting: boolean;
  participantCount: number;
  minimal: boolean;
}

/** Mirrors `menubar::MicMuteChanged` (src-tauri/src/menubar.rs). */
export interface MicMuteChanged {
  muted: boolean;
}

/**
 * Mirrors native `CameraPublishStateEvent`: `publishing: false` with an
 * `error` means camera loss or a live device-switch failure cleared native
 * camera intent, so UI must drop its ON state and show retry.
 */
export interface CameraPublishState {
  publishing: boolean;
  error: string | null;
}

/** Mirrors native `StartCameraPublishResult`. `published: true` means capture
 * delivered a first frame and LiveKit publication succeeded; `published:
 * false` means the immediate attempt failed but the bounded self-heal loop is
 * retrying in the background (terminal outcome arrives as a
 * `camera-publish-state` event). */
export interface StartCameraPublishResult {
  published: boolean;
}

/** Mirrors native `CameraPublishStateSnapshot` -- lets a (re)mounting meeting
 * route sync its Video toggle + self-view to the real native camera state. */
export interface CameraPublishStateSnapshot {
  publishing: boolean;
  intended: boolean;
}

export function cameraPublishSyncPlan(snapshot: CameraPublishStateSnapshot): {
  activate: boolean;
  acquirePreview: boolean;
} {
  return {
    activate: snapshot.publishing,
    // Both platforms always have a webview self-view now (Windows: the
    // native-fed canvas stream; macOS: getUserMedia), so any camera intent —
    // publishing OR still retrying — wants the preview up.
    acquirePreview: snapshot.publishing || snapshot.intended
  };
}

export interface MeetingTeardownPlan {
  /** Release the local getUserMedia self-view preview (camera light). Always safe. */
  releaseSelfViewPreview: boolean;
  /** Stop the NATIVE camera publish other participants are watching. */
  stopCameraPublish: boolean;
}

/**
 * #782: the meeting route unmounts for reasons that are NOT leaving the room (the menubar
 * popover's Settings row navigates the main webview). Stopping the native publish there
 * froze a real user's camera for 73s while they were still in the meeting.
 */
export function meetingTeardownPlan(input: { stillJoined: boolean }): MeetingTeardownPlan {
  return {
    releaseSelfViewPreview: true,
    stopCameraPublish: !input.stillJoined
  };
}

/** Mirrors `permissions::auth_status_string` (src-tauri/src/permissions.rs). */
export type AuthStatus = 'not-determined' | 'restricted' | 'denied' | 'authorized';

/** Mirrors `permissions::PermissionRequestOutcome` (src-tauri/src/permissions.rs). */
export interface PermissionRequestOutcome {
  granted: boolean;
  wasGranted: boolean;
  autoRelaunchRecommended: boolean;
}

/** Mirrors `transport::audio::AudioDeviceInfo` (src-tauri/src/transport/audio.rs). */
export interface AudioDeviceInfo {
  id: string;
  name: string;
}

/** Mirrors `transport::audio::AudioDeviceLists` (src-tauri/src/transport/audio.rs). */
export interface AudioDeviceLists {
  recording: AudioDeviceInfo[];
  playout: AudioDeviceInfo[];
}

/** Mirrors `transport::audio::AppliedAudioDevices` (src-tauri/src/transport/audio.rs). */
export interface AppliedAudioDevices {
  micApplied: boolean;
  speakerApplied: boolean;
  inRoom: boolean;
  micError: string | null;
  speakerError: string | null;
}

/** Mirrors native platform camera IDs (not browser-salted WebKit/WebView2 deviceIds). */
export interface CameraDeviceInfo {
  id: string;
  name: string;
}

/** One (width, height, frame-rate) mode a camera can deliver (camelCase from
 * the Rust `CameraMode`); used to grey out unsupported resolution/FPS presets. */
export interface CameraMode {
  width: number;
  height: number;
  frameRateNumerator: number;
  frameRateDenominator: number;
}

export interface AppliedCameraDevice {
  applied: boolean;
  inRoom: boolean;
  usedDefaultFallback: boolean;
  error: string | null;
}

/** Mirrors `gallery_bridge::GalleryBridgeConfig` (src-tauri/src/gallery_bridge.rs). */
export interface GalleryBridgeConfig {
  url: string;
  token: string;
  livekitRoom: string;
  identity: string;
}

/**
 * Mirrors `feedback::FeedbackDiagnostics` (src-tauri/src/feedback.rs) --
 * the bounded, redacted, opt-in diagnostic zip offered as a UserDispatch
 * feedback attachment (#292). `bytesBase64` is the whole zip, never a path.
 */
export interface FeedbackDiagnostics {
  filename: string;
  mimeType: string;
  bytesBase64: string;
  byteCount: number;
}

/** Mirrors `hover_core::HoverTabUpdate` (src-tauri/src/hover_core.rs). */
export interface HoverTabUpdate {
  windowId: number;
  frame: WindowFrame;
  tabX: number;
  tabY: number;
  attachment: 'outside' | 'inset';
  verticalOffset: number;
  shared: boolean;
  displayLike: boolean;
}

/** Mirrors `hover_core::HoverTabDragPhase` (closed native drag vocabulary). */
export type HoverTabDragPhase = 'begin' | 'update' | 'commit' | 'cancel';

export interface ShareStateChanged {
  windowId: number;
  shared: boolean;
}

export type ShareControlMode = 'cursorPreserving' | 'fullControl';

export interface ShareControlModeChanged {
  windowId: number;
  controlMode: ShareControlMode;
}

export interface RegionShareState {
  active: boolean;
}

export interface RegionViewOptionsState {
  shareActive: boolean;
  priority: SharePriority;
  drawActive: boolean;
  aiChatEnabled: boolean;
  aiChatActive: boolean;
  controllerName: string | null;
}

export interface RegionViewOptionsChanged {
  selectorLabel: string;
  state: RegionViewOptionsState;
}

export interface RegionControlStateChanged {
  selectorLabel: string;
  active: boolean;
  controllerName: string | null;
}

export interface RegionShareStateChanged {
  windowId: number;
  selectorLabel: string;
  active: boolean;
}

/** Mirrors `share_notice::RemoteShareStartedPayload` (src-tauri/src/share_notice.rs). */
export interface RemoteShareStartedEvent {
  windowId: number;
  ownerIdentity: string;
  ownerDisplayName: string;
  sourceTitle: string;
}

export type SharePriority = 'automatic' | 'responsive' | 'sharpText' | 'dataSaver';

/** Mirrors `hover_tab::ShareErrorPayload` (src-tauri/src/hover_tab.rs). */
export interface ShareErrorPayload {
  windowId: number;
  wasStarting: boolean;
  error: ShareSessionError;
}

export type PointerActivity = 'click' | 'type';

/** Mirrors `telepointer::TelepointerUpdate` (src-tauri/src/telepointer.rs). */
export interface TelepointerUpdate {
  windowId: number;
  userId: string;
  surfaceOwnerId?: string;
  displayName?: string;
  paletteIndex?: number | null;
  x: number;
  y: number;
  visible: boolean;
  activity?: PointerActivity;
}

export type DrawMessageKind = 'begin' | 'points' | 'end' | 'clear' | 'text';

export interface DrawPoint {
  x: number;
  y: number;
}

/** Mirrors `draw::DrawDraft` (src-tauri/src/draw.rs). */
export interface DrawDraft {
  type: DrawMessageKind;
  windowId: number;
  ownerIdentity: string;
  strokeId: string | null;
  seq: number;
  points?: DrawPoint[];
  text?: string;
}

/** Mirrors `draw::DrawUpdate` (src-tauri/src/draw.rs). */
export interface DrawUpdate extends DrawDraft {
  drawerIdentity: string;
  drawerDisplayName?: string | null;
  drawerPaletteIndex?: number | null;
}

export type RemoteControlKind = 'request' | 'release' | 'pointer' | 'wheel' | 'key' | 'text';
export type RemoteControlAction = 'move' | 'down' | 'up' | 'click';

/** Mirrors `remote_control::RemoteControlModifiers` (src-tauri/src/remote_control.rs). */
export interface RemoteControlModifiers {
  alt: boolean;
  ctrl: boolean;
  meta: boolean;
  shift: boolean;
}
export type RemoteControlTargetKind = 'window' | 'display';
export type RemoteControlCapability =
  | 'legacyControl'
  | 'discretePointerV1'
  | 'discreteScrollV1'
  | 'windowLocalPointer'
  | 'globalKeyboard'
  | 'uiaInvoke'
  | 'uiaScroll'
  | 'unicodeText';
export type RemoteControlReason =
  | 'controllerUpgradeRequired'
  | 'requestEscalation'
  | 'consentDenied'
  | 'consentTimedOut';

/** Mirrors `remote_control_core::RemoteControlPolicy`. Host-side authority,
 * never on the wire: `off` refuses, `ask` (default) prompts the sharer,
 * `auto` grants any authenticated in-room requester immediately. */
export type RemoteControlPolicy = 'off' | 'ask' | 'auto';

/** Mirrors `remote_control::ControlConsentRequestedPayload`. */
export type ControlConsentRequestedEvent =
  | {
      kind: 'control';
      windowId: number;
      controllerId: string;
      controllerName: string;
      windowTitle?: string;
      timeoutMs: number;
    }
  | {
      kind: 'fullControlEscalation';
      windowId: number;
      controllerId: string;
      controllerName: string;
      windowTitle?: string;
      timeoutMs: number;
    };


/** Mirrors `remote_control::RemoteControlDraft` (src-tauri/src/remote_control.rs). */
export interface RemoteControlDraft {
  kind: RemoteControlKind;
  action?: RemoteControlAction;
  windowId: number;
  targetOwnerId?: string;
  seq: number;
  targetKind?: RemoteControlTargetKind;
  shareInstanceId?: string;
  controllerCapabilities?: RemoteControlCapability[];
  grantToken?: string;
  x?: number;
  y?: number;
  button?: number;
  buttons?: number;
  /** #373: authoritative multi-click count (mirrors DOM `detail`), additive/optional. */
  clickCount?: number;
  deltaX?: number;
  deltaY?: number;
  deltaMode?: number;
  key?: string;
  code?: string;
  repeat?: boolean;
  location?: number;
  text?: string;
  modifiers?: RemoteControlModifiers;
}

/** Mirrors `remote_control::RemoteControlStatus` (src-tauri/src/remote_control.rs). */
export interface RemoteControlStatus {
  windowId: number;
  ownerIdentity?: string | null;
  controllerId: string;
  grantToken?: string | null;
  targetKind?: RemoteControlTargetKind;
  shareInstanceId?: string;
  controllerCapabilities?: RemoteControlCapability[];
  hostCapabilities?: RemoteControlCapability[];
  reason?: RemoteControlReason;
  status:
    | 'active'
    | 'stopped'
    | 'disabled'
    | 'accessibilityDenied'
    | 'requestFailed'
    | 'textTruncated'
    | 'targetPaused'
    | 'targetUnavailable'
    | 'requestUnavailable'
    | 'notForeground'
    | 'occluded'
    | 'integrityBlocked'
    | 'secureField'
    | 'unsupportedRoute'
    | 'staleShareInstance'
    | 'injectionTimeout'
    | 'awaitingConsent'
    | 'denied'
    | (string & {});
  message: string;
}

/** Mirrors `resilience_event::ResilienceEvent` (src-tauri/src/resilience_event.rs). */
export type ResilienceEvent =
  | { kind: 'reconnecting' }
  | { kind: 'reconnected'; message: string }
  | { kind: 'disconnected'; reason: string }
  | { kind: 'networkChanged' }
  | { kind: 'micDeviceChanged'; deviceName: string; usingDefault?: boolean }
  | { kind: 'micDeviceFailed'; message: string }
  | { kind: 'speakerDeviceChanged'; deviceName: string; usingDefault?: boolean }
  | { kind: 'speakerDeviceFailed'; message: string }
  | { kind: 'sharePublicationRepairRecovering'; windowId: number }
  | { kind: 'sharePublicationRepairCancelled'; windowId: number }
  | { kind: 'sharePublicationRepairRestored'; windowId: number }
  | { kind: 'sharePublicationRepairFailed'; windowId: number; message: string }
  | { kind: 'micPublicationRepairFailed'; message: string }
  | { kind: 'cameraPublicationRepairFailed'; message: string };

/** Mirrors `diagnostics::StatsSample` (src-tauri/src/diagnostics.rs). */
export interface StatsSample {
  tMs: number;
  rttMs: number | null;
  jitterMs: number | null;
  sendKbps: number;
  recvKbps: number;
  lossPct: number | null;
  glassToGlassMs?: number | null;
  glassToGlassEstimateMs?: number | null;
  availableOutgoingKbps?: number | null;
  availableIncomingKbps?: number | null;
  cpuPct?: number | null;
  memoryPct?: number | null;
  thermalState?: string | null;
  /** #683: process-wide phys_footprint (macOS)/PrivateUsage (Windows), MB. */
  physFootprintMb?: number | null;
  /** #683: live count of this app's own decode-output CVPixelBuffers (macOS only). */
  livePixelBuffers?: number | null;
}

/** Mirrors `diagnostics::TrackHealth` (src-tauri/src/diagnostics.rs). */
export interface PipelineStageMetrics {
  width: number | null;
  height: number | null;
  fps: number | null;
  kbps: number | null;
}

export type CaptureStateKind = 'live' | 'idle' | 'occluded' | 'wedged';

export interface CaptureCpuMetrics {
  lockCopyMs: number | null;
  convertMs: number | null;
  captureFrameReturnMs: number | null;
}

export interface CaptureStateReport {
  state: CaptureStateKind;
  fps: number | null;
  dirtyRectCount: number | null;
  dirtyAreaPx: number | null;
  occlusionPct: number | null;
  cpu: CaptureCpuMetrics;
}

export interface ReceiverFreezeMetrics {
  freezeCount: number;
  framesDropped: number;
  qualityLimitationReason: string | null;
}

export interface PipelineStageReport {
  reporterId: string;
  sentAtMs: number;
  receivedAtMs: number;
  metrics: PipelineStageMetrics;
}

export interface RemoteCaptureStateReport {
  reporterId: string;
  sentAtMs: number;
  receivedAtMs: number;
  state: CaptureStateReport;
}

export interface RemoteReceiverFreezeReport {
  reporterId: string;
  sentAtMs: number;
  receivedAtMs: number;
  metrics: ReceiverFreezeMetrics;
}

export interface RemotePipelineLifecycleReport {
  reporterId: string;
  lifecycle: string;
  receivedAtMs: number;
}

export type NativeStartupStageKind =
  | 'startRequested'
  | 'captureAttemptStarted'
  | 'firstFrame'
  | 'firstFrameTimeout'
  | 'metadataStarted'
  | 'metadataWithinBudget'
  | 'metadataBudgetExpired'
  | 'publishStarted'
  | 'publishSucceeded'
  | 'publishFailed'
  | 'firstFramePushed'
  | 'snapshotPullStarted'
  | 'snapshotPullPushed'
  | 'snapshotPullFailed';

export interface NativeStartupStage {
  stage: NativeStartupStageKind;
  elapsedMs: number;
  width: number | null;
  height: number | null;
  fps: number | null;
  resolution: string | null;
  capturePath: string | null;
  detail: string | null;
}

export interface NativeStartupTimelineReport {
  windowId: number;
  startedSeq: number | null;
  restartGeneration: number | null;
  capturePath: string;
  requestedFps: number | null;
  requestedResolution: string | null;
  publicationSid: string | null;
  outcome: 'in-progress' | 'published' | 'publish-failed' | 'capture-failed' | (string & {});
  stages: NativeStartupStage[];
}

export interface TrackHealth {
  sid: string;
  name: string;
  rawTrackName?: string | null;
  ownerIdentity?: string | null;
  windowId?: number | null;
  kind: string;
  direction: string;
  width: number;
  height: number;
  fps: number;
  codecImpl: string;
  qualityLimitation: string;
  softwareEncoder: boolean;
  targetKbps: number;
  actualKbps: number;
  packetsLost: number;
  framesEncoded: number;
  keyFramesEncoded: number;
  framesDecoded: number;
  keyFramesDecoded: number;
  framesDropped: number;
  nackCount: number;
  firCount: number;
  pliCount: number;
  jitterBufferMs: number | null;
  jitterBufferTargetMs?: number | null;
  jitterBufferMinimumMs?: number | null;
  glassToGlassMs: number | null;
  glassToGlassEstimateMs: number | null;
  glassToGlassStatus?: 'calibrated' | 'clock-sync-pending' | '' | (string & {});
  streamState: 'active' | 'paused' | 'stalled' | 'unknown' | (string & {});
  grabbed?: PipelineStageMetrics | null;
  encodedSent?: PipelineStageMetrics | null;
  received?: PipelineStageMetrics | null;
  decoded?: PipelineStageMetrics | null;
  displayEnqueued?: PipelineStageMetrics | null;
  captureState?: CaptureStateReport | null;
  receiverFreeze?: ReceiverFreezeMetrics | null;
  remoteGrabbed?: PipelineStageReport | null;
  remoteEncodedSent?: PipelineStageReport | null;
  remoteReceived?: PipelineStageReport | null;
  remoteDecoded?: PipelineStageReport | null;
  remoteCaptureState?: RemoteCaptureStateReport | null;
  remoteReceiverFreeze?: RemoteReceiverFreezeReport | null;
  remoteLifecycle?: RemotePipelineLifecycleReport | null;
  availableKbps?: number | null;
}

export interface SystemSignals {
  cpuPct?: number | null;
  memoryPct?: number | null;
  thermalState?: string | null;
  thermalPressure?: string | null;
}

/** Mirrors `diagnostics::ParticipantQuality` (src-tauri/src/diagnostics.rs). */
export interface ParticipantQuality {
  identity: string;
  quality: string;
}

/** Mirrors `diagnostics::AnalysisFinding` (src-tauri/src/diagnostics.rs). */
export interface AnalysisFinding {
  severity: 'info' | 'warn' | (string & {});
  title: string;
  evidence: string;
  recommendation: string;
}

/** Mirrors `diagnostics::JournalEntry` (src-tauri/src/diagnostics.rs). */
export interface JournalEntry {
  tMs: number;
  category: string;
  message: string;
}

/** Mirrors `diagnostics::NetworkSnapshot` (src-tauri/src/diagnostics.rs). */
export interface NetworkSnapshot {
  connected: boolean;
  roomName: string | null;
  serverHost: string | null;
  localIdentity: string | null;
  reconnectCount: number;
  quality: ParticipantQuality[];
  peerRttMs: number | null;
  history: StatsSample[];
  tracks: TrackHealth[];
  nativeStartup: NativeStartupTimelineReport[];
  analysis: AnalysisFinding[];
  glassToGlassMs?: number | null;
  glassToGlassEstimateMs?: number | null;
  availableOutgoingKbps?: number | null;
  availableIncomingKbps?: number | null;
  system?: SystemSignals | null;
}

/** Mirrors `compositor::RemoteWindowDebugStats` (src-tauri/src/compositor.rs). */
export interface RemoteWindowDebugStats {
  windowId: number;
  ownerIdentity: string;
  ownerDisplayName: string;
  sourceTitle: string;
  sourceUrl: string | null;
  contentWidth: number;
  contentHeight: number;
  receiverScale: number;
  displayPixelWidth: number;
  displayPixelHeight: number;
  sourcePixelWidth: number | null;
  sourcePixelHeight: number | null;
  lastFrameReceivedMs: number | null;
  framesReceived: number;
  lastDisplayEnqueuedMs: number | null;
  framesDisplayEnqueued: number;
  remoteControlAvailable: boolean;
}

export interface CommandArgs {
  [COMMANDS.animateMainWindowResize]: { width: number; height: number };
  [COMMANDS.captureWindowThumbnail]: { windowId: number; force?: boolean };
  [COMMANDS.cockpitStatus]: Record<string, never>;
  [COMMANDS.compositorActivateWindow]: { windowId: number; ownerIdentity?: string };
  [COMMANDS.compositorRaiseWindowForClick]: {
    windowId: number;
    ownerIdentity?: string;
    keyControlChild: boolean;
  };
  [COMMANDS.compositorRaiseParticipantWindows]: { ownerIdentity: string };
  [COMMANDS.compositorBeginResize]: { windowId: number; ownerIdentity?: string };
  [COMMANDS.compositorFitToSource]: { windowId: number; ownerIdentity?: string };
  [COMMANDS.compositorHideWindow]: { windowId: number; ownerIdentity?: string };
  [COMMANDS.compositorPopOut]: { windowId: number; ownerIdentity?: string };
  [COMMANDS.compositorResizeWindow]: {
    windowId: number;
    ownerIdentity?: string;
    direction: string;
    startX: number;
    startY: number;
    startWidth: number;
    startHeight: number;
    deltaX: number;
    deltaY: number;
    finalize?: boolean;
  };
  [COMMANDS.aiChatControlApprove]: {
    sessionId: number;
    requestId: string;
    /** Explicit escalation. False (the default the UI offers first) authorizes one action. */
    sessionScope: boolean;
  };
  [COMMANDS.aiChatControlReject]: { sessionId: number };
  [COMMANDS.aiChatControlResume]: { sessionId: number };
  [COMMANDS.aiChatControlStatus]: Record<string, never>;
  [COMMANDS.aiChatIsActive]: { windowId: number };
  [COMMANDS.aiChatPanelDismiss]: Record<string, never>;
  [COMMANDS.aiChatPanelPresent]: { windowId: number };
  // The receiver half (#657): a window someone ELSE shares is addressed by
  // (windowId, ownerIdentity), never by windowId alone — window ids are only
  // unique per owner, so dropping the owner would let one participant's request
  // land on a different participant's window.
  [COMMANDS.aiChatRemoteSession]: { windowId: number; ownerIdentity: string };
  [COMMANDS.aiChatRequestStart]: { windowId: number; ownerIdentity: string };
  [COMMANDS.aiChatRequestStop]: { windowId: number; ownerIdentity: string };
  [COMMANDS.aiChatSetApiKey]: { key: string | null };
  [COMMANDS.aiChatSetEnabled]: { enabled: boolean };
  [COMMANDS.aiChatStart]: { windowId: number };
  [COMMANDS.compositorAiChatOverlayIsOpen]: { windowId: number; ownerIdentity?: string };
  [COMMANDS.compositorSetAiChatOverlayOpen]: { windowId: number; ownerIdentity?: string; open: boolean };
  [COMMANDS.compositorSetDrawActive]: { windowId: number; ownerIdentity?: string; active: boolean };
  [COMMANDS.compositorStartDrag]: { windowId: number; ownerIdentity?: string };
  [COMMANDS.compositorToggleDebugPanel]: { windowId: number; ownerIdentity?: string };
  [COMMANDS.compositorWindowDebugStats]: { windowId: number; ownerIdentity?: string };
  [COMMANDS.createRoom]: { name: string; open: boolean; displayName?: string | null };
  [COMMANDS.debugModeSettings]: Record<string, never>;
  [COMMANDS.drawSend]: { draft: DrawDraft };
  [COMMANDS.downloadAndInstallCompatibleUpdate]: Record<string, never>;
  [COMMANDS.runLaunchUpdateCheck]: Record<string, never>;
  [COMMANDS.forgetRoom]: { idOrCode: string };
  [COMMANDS.frontendReady]: { windowLabel: string };
  [COMMANDS.nextSelfViewFrame]: Record<string, never>;
  [COMMANDS.galleryBridgeConfig]: { roomName: string; identity: string };
  [COMMANDS.getTestCockpitArtifactDataUrl]: { resultsDir: string; path: string };
  [COMMANDS.getTestCockpitRun]: { resultsDir: string };
  [COMMANDS.joinRoom]: {
    roomName: string;
    identity: string;
    displayName: string;
    remoteControlAllowed: boolean;
    remoteControlPolicy?: RemoteControlPolicy;
  };
  [COMMANDS.openMainRoute]: { route: string };
  [COMMANDS.openTestCockpitResultsFolder]: { path: string };
  [COMMANDS.openWindowPickerWindow]: { color?: string };
  [COMMANDS.toggleWindowPickerWindow]: Record<string, never>;
  [COMMANDS.recordVideoStreamState]: {
    participantIdentity: string;
    trackName: string;
    state: string;
    source: string;
  };
  [COMMANDS.recordCameraReceiveHealth]: {
    cadence: 'reduced' | 'severe' | 'stalled';
    decoderRender: 'decoder_degraded';
  };
  [COMMANDS.remoteControlRevoke]: { windowId: number; controllerId: string };
  [COMMANDS.remoteControlRequestTimedOut]: { windowId: number; ownerIdentity?: string };
  [COMMANDS.remoteClipboardCopy]: { windowId: number; ownerIdentity?: string; grantToken?: string };
  [COMMANDS.remoteClipboardPaste]: { windowId: number; ownerIdentity?: string; grantToken?: string };
  // #905: date range for the export -- omitted/undefined defaults to the
  // last 2 days server-side; 0 means "all logs, no filtering". See
  // `logging::export_logs`'s doc comment (apps/desktop/src-tauri).
  [COMMANDS.exportLogs]: { days?: number };
  [COMMANDS.remoteControlSend]: { draft: RemoteControlDraft };
  [COMMANDS.remoteControlSetActive]: { windowId: number; ownerIdentity?: string; active: boolean };
  [COMMANDS.renameRoom]: { idOrCode: string; displayName: string | null };
  [COMMANDS.resetLocalRooms]: Record<string, never>;
  [COMMANDS.resizeMenubarPopover]: { height: number };
  [COMMANDS.restartApp]: { reason?: string };
  [COMMANDS.openPrivacySettings]: {
    which: 'screenRecording' | 'microphone' | 'camera' | 'accessibility';
  };
  [COMMANDS.setAudioDevices]: { recordingId: string | null; playoutId: string | null };
  [COMMANDS.listCameraModes]: { preferredDeviceId: string | null };
  [COMMANDS.setCameraDevice]: { deviceId: string };
  [COMMANDS.setCameraPrefs]: {
    width: number | null;
    height: number | null;
    frameRate: number | null;
  };
  [COMMANDS.setDebugMode]: { enabled: boolean };
  [COMMANDS.setMainPillMode]: { active: boolean };
  [COMMANDS.startTestCockpit]: { args: { selector: string } };
  [COMMANDS.cancelTestCockpit]: Record<string, never>;
  [COMMANDS.setCockpitOpen]: { open: boolean };
  [COMMANDS.setMicMuted]: { muted: boolean };
  [COMMANDS.setRemoteControlAllowed]: { allowed: boolean };
  [COMMANDS.setRemoteControlPolicy]: { policy: RemoteControlPolicy };
  [COMMANDS.remoteControlAnswerConsent]: { windowId: number; controllerId: string; approve: boolean };
  [COMMANDS.remoteControlAnswerEscalation]: { windowId: number; controllerId: string; approve: boolean };
  [COMMANDS.setSentryEnabled]: { enabled: boolean };
  [COMMANDS.controlConsentDismiss]: Record<string, never>;
  /** Measured content height, same resize-to-content pattern as shareNoticePresent. */
  [COMMANDS.controlConsentPresent]: { height: number };
  [COMMANDS.closeRegionWindow]: { windowLabel: string };
  [COMMANDS.openRegionWindow]: { userName?: string | null; followCursor?: boolean };
  [COMMANDS.shareNoticeDismiss]: Record<string, never>;
  /** `height` is the page's own measured content height (logical points) --
   * see `share-notice/+page.svelte`'s `reportHeight`, same resize-to-content
   * pattern as `COMMANDS.resizeMenubarPopover`. */
  [COMMANDS.shareNoticePresent]: { height: number };
  [COMMANDS.shareOverlaySetDrawActive]: { windowId: number; active: boolean };
  [COMMANDS.setHoverTabMenuOpen]: { open: boolean };
  [COMMANDS.regionPlacementActive]: { windowLabel: string };
  [COMMANDS.regionShareState]: { windowLabel: string };
  [COMMANDS.syncRegionWindowFrame]: { windowLabel: string };
  [COMMANDS.regionViewOptionsState]: { windowLabel: string };
  [COMMANDS.setRegionSharePriority]: { windowLabel: string; priority: SharePriority };
  [COMMANDS.setRegionDrawActive]: { windowLabel: string; active: boolean };
  [COMMANDS.regionAiChatStart]: { windowLabel: string };
  [COMMANDS.regionAiChatStop]: { windowLabel: string };
  [COMMANDS.toggleRegionShare]: { windowLabel: string; color?: string };
  [COMMANDS.shareWindow]: { windowId: number; color?: string; controlMode?: string };
  [COMMANDS.setShareControlMode]: { windowId: number; controlMode?: string };
  [COMMANDS.setShareRemoteControlAllowed]: { windowId: number; allowed: boolean };
  [COMMANDS.shareRemoteControlAllowed]: { windowId: number };
  [COMMANDS.toggleWindowShare]: { windowId: number; frame: WindowFrame; color?: string };
  [COMMANDS.updateShareBorderFrame]: {
    borderId: number;
    x: number;
    y: number;
    width: number;
    height: number;
  };
}

export interface CommandReturns {
  [COMMANDS.autotestJoinResult]: AutotestJoinResult | null;
  [COMMANDS.startCameraPublish]: StartCameraPublishResult;
  [COMMANDS.cameraPublishState]: CameraPublishStateSnapshot;
  [COMMANDS.animateMainWindowResize]: AnimatedResizeOutcome;
  [COMMANDS.captureWindowThumbnail]: string;
  [COMMANDS.closeRegionWindow]: void;
  [COMMANDS.checkAccessibility]: boolean;
  [COMMANDS.checkCamera]: AuthStatus;
  [COMMANDS.checkCompatibleUpdateAvailable]: {
    status: 'up-to-date' | 'available';
    version: string | null;
  };
  [COMMANDS.checkMicrophone]: AuthStatus;
  [COMMANDS.checkScreenRecording]: boolean;
  [COMMANDS.cockpitStatus]: CockpitStatus;
  [COMMANDS.compositorBeginResize]: {
    x: number;
    y: number;
    width: number;
    height: number;
  };
  /** False when the answer was stale — a different request, or a finished session. */
  [COMMANDS.aiChatControlApprove]: boolean;
  [COMMANDS.aiChatControlReject]: boolean;
  [COMMANDS.aiChatControlResume]: boolean;
  [COMMANDS.aiChatControlStatus]: AiChatControlStatus | null;
  [COMMANDS.aiChatIsActive]: boolean;
  [COMMANDS.aiChatPanelDismiss]: void;
  [COMMANDS.aiChatPanelPresent]: AiChatPanelInfo;
  [COMMANDS.aiChatPttEnd]: void;
  [COMMANDS.aiChatPttStart]: boolean;
  /** `null` when this client knows of no session for that window. */
  [COMMANDS.aiChatRemoteSession]: AiChatRemoteSessionState | null;
  [COMMANDS.aiChatRequestPttEnd]: void;
  [COMMANDS.aiChatRequestPttStart]: void;
  [COMMANDS.aiChatRequestSendText]: void;
  [COMMANDS.aiChatRequestStart]: void;
  [COMMANDS.aiChatRequestStop]: void;
  [COMMANDS.aiChatSendText]: boolean;
  [COMMANDS.aiChatSetApiKey]: AiChatSettings;
  [COMMANDS.aiChatSetEnabled]: AiChatSettings;
  [COMMANDS.aiChatSettings]: AiChatSettings;
  [COMMANDS.aiChatStart]: AiChatStartOutcome;
  [COMMANDS.aiChatStop]: void;
  [COMMANDS.compositorAiChatOverlayIsOpen]: boolean;
  [COMMANDS.compositorSetAiChatOverlayOpen]: void;
  [COMMANDS.compositorSetDrawActive]: void;
  [COMMANDS.compositorWindowDebugStats]: RemoteWindowDebugStats;
  [COMMANDS.createRoom]: RoomRecord;
  [COMMANDS.debugModeSettings]: DebugModeSettings;
  [COMMANDS.setDebugMode]: DebugModeSettings;
  [COMMANDS.setMainPillMode]: void;
  [COMMANDS.drawSend]: void;
  [COMMANDS.currentRoom]: string | null;
  [COMMANDS.downloadAndInstallCompatibleUpdate]: {
    status: 'up-to-date' | 'installed';
    version: string | null;
  };
  [COMMANDS.forgetRoom]: RoomRecord;
  [COMMANDS.frontendReady]: void;
  [COMMANDS.prepareFeedbackDiagnostics]: FeedbackDiagnostics;
  [COMMANDS.recordCameraReceiveHealth]: boolean;
  [COMMANDS.galleryBridgeConfig]: GalleryBridgeConfig;
  [COMMANDS.getBuildInfo]: BuildInfo;
  [COMMANDS.getEventJournal]: JournalEntry[];
  [COMMANDS.getMenubarState]: MenubarPillState;
  [COMMANDS.getNetworkSnapshot]: NetworkSnapshot;
  [COMMANDS.getTestCockpitArtifactDataUrl]: string;
  [COMMANDS.getTestCockpitRun]: TestCockpitRunDetail;
  [COMMANDS.hoverTabPageMounted]: HoverTabUpdate | null;
  [COMMANDS.openRegionWindow]: string;
  [COMMANDS.joinRoom]: RoomRecord;
  [COMMANDS.listAudioDevices]: AudioDeviceLists;
  [COMMANDS.listCameraDevices]: CameraDeviceInfo[];
  [COMMANDS.listCameraModes]: CameraMode[];
  [COMMANDS.listRoomOccupancy]: RoomOccupancy[];
  [COMMANDS.listRooms]: RoomRecord[];
  [COMMANDS.listShareableWindows]: ShareableWindow[];
  [COMMANDS.nextSelfViewFrame]: ArrayBuffer;
  [COMMANDS.listTestCockpitRuns]: TestCockpitRunSummary[];
  [COMMANDS.openPrivacySettings]: boolean;
  [COMMANDS.openTestCockpitResultsFolder]: boolean;
  [COMMANDS.remoteControlAllowed]: boolean;
  [COMMANDS.remoteControlPolicy]: RemoteControlPolicy;
  [COMMANDS.remoteClipboardCopy]: void;
  [COMMANDS.remoteClipboardPaste]: void;
  [COMMANDS.remoteControlAnswerConsent]: boolean;
  [COMMANDS.remoteControlSetActive]: boolean;
  [COMMANDS.regionPlacementActive]: boolean;
  [COMMANDS.regionShareState]: RegionShareState;
  [COMMANDS.syncRegionWindowFrame]: void;
  [COMMANDS.regionViewOptionsState]: RegionViewOptionsState;
  [COMMANDS.setRegionSharePriority]: SharePriority;
  [COMMANDS.setRegionDrawActive]: boolean;
  [COMMANDS.regionAiChatStart]: AiChatStartOutcome;
  [COMMANDS.regionAiChatStop]: boolean;
  [COMMANDS.toggleRegionShare]: boolean;
  [COMMANDS.compositorRaiseWindowForClick]: void;
  [COMMANDS.renameRoom]: RoomRecord;
  [COMMANDS.resetLocalRooms]: void;
  [COMMANDS.requestAccessibility]: PermissionRequestOutcome;
  [COMMANDS.requestCamera]: AuthStatus;
  [COMMANDS.requestMicrophone]: AuthStatus;
  [COMMANDS.requestScreenRecording]: PermissionRequestOutcome;
  [COMMANDS.restartApp]: boolean;
  [COMMANDS.roomPresence]: PresentParticipant[];
  [COMMANDS.setAudioDevices]: AppliedAudioDevices;
  [COMMANDS.setCameraDevice]: AppliedCameraDevice;
  [COMMANDS.setCameraPrefs]: AppliedCameraDevice;
  [COMMANDS.startTestCockpit]: CockpitStatus;
  [COMMANDS.cancelTestCockpit]: CockpitStatus;
  [COMMANDS.setMicMuted]: boolean;
  [COMMANDS.setRemoteControlAllowed]: boolean;
  [COMMANDS.setRemoteControlPolicy]: RemoteControlPolicy;
  [COMMANDS.remoteControlAnswerEscalation]: boolean;
  [COMMANDS.setSentryEnabled]: void;
  [COMMANDS.shareOverlaySetDrawActive]: void;
  [COMMANDS.setHoverTabMenuOpen]: void;
  [COMMANDS.shareWindow]: boolean;
  [COMMANDS.sharedWindowIds]: number[];
  [COMMANDS.toggleMenubarMic]: boolean;
  [COMMANDS.toggleWindowShare]: boolean;
}

export interface EventPayloads {
  [EVENTS.aiChatControlRequest]: AiChatControlRequestEvent;
  [EVENTS.aiChatControlResolved]: AiChatControlResolvedEvent;
  [EVENTS.aiChatRemoteState]: AiChatRemoteSessionState;
  [EVENTS.aiChatRemoteTranscript]: AiChatRemoteTranscriptEvent;
  [EVENTS.aiChatOverlayOpenChanged]: AiChatOverlayOpenChangedEvent;
  [EVENTS.aiChatState]: AiChatStateEvent;
  [EVENTS.aiChatTranscript]: AiChatTranscriptEvent;
  [EVENTS.aiChatRefused]: AiChatRefusedEvent;
  [EVENTS.autotestJoinResult]: AutotestJoinResult;
  [EVENTS.cameraPublishState]: CameraPublishState;
  [EVENTS.debugModeChanged]: DebugModeSettings;
  [EVENTS.desktopWindowsChanged]: void;
  [EVENTS.hoverTabHide]: void;
  [EVENTS.hoverTabUpdate]: HoverTabUpdate;
  [EVENTS.drawUpdate]: DrawUpdate;
  [EVENTS.journalAppended]: JournalEntry;
  [EVENTS.meetingRestorePillRequested]: void;
  [EVENTS.micMuteChanged]: MicMuteChanged;
  [EVENTS.networkStats]: NetworkSnapshot;
  [EVENTS.presenceUpdate]: PresenceUpdate;
  [EVENTS.regionPlacementSettled]: { selectorLabel: string };
  [EVENTS.regionPlacementReleased]: { selectorLabel: string };
  [EVENTS.regionShareStateChanged]: RegionShareStateChanged;
  [EVENTS.regionViewOptionsChanged]: RegionViewOptionsChanged;
  [EVENTS.regionControlStateChanged]: RegionControlStateChanged;
  [EVENTS.controlConsentRequested]: ControlConsentRequestedEvent;
  [EVENTS.remoteControlStatus]: RemoteControlStatus;
  [EVENTS.remoteShareStarted]: RemoteShareStartedEvent;
  [EVENTS.resilienceEvent]: ResilienceEvent;
  [EVENTS.roomLeft]: RoomLeftEvent;
  [EVENTS.shareError]: ShareErrorPayload;
  [EVENTS.shareStateChanged]: ShareStateChanged;
  [EVENTS.shareControlModeChanged]: ShareControlModeChanged;
  [EVENTS.sharePickerChanged]: void;
  [EVENTS.sharePickerOpened]: void;
  [EVENTS.sharePickerVisibilityChanged]: { open: boolean };
  [EVENTS.telepointerUpdate]: TelepointerUpdate;
  [EVENTS.testProgress]: TestProgressEvent;
}
