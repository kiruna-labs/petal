// Frontend-only session/identity store.
//
// STAND-IN for onboarding-completion persistence: there is no real
// persisted-onboarding concept from the Rust/native side yet (no
// `src-tauri` command for it, no on-disk profile) -- `onboardingComplete`/
// name/color are `localStorage`-backed so the app can be clicked through as
// a coherent flow today, without re-onboarding on every reload.
//
// TODO: replace `onboardingComplete` with real persisted onboarding state
// once native onboarding state exists (e.g. a Tauri command backed by a
// real on-disk profile or keychain-backed identity, per SPEC.md's eventual
// persistence story).
//
// NOTE: as of the room-join-flow task, `name`/`identity`/`participantId`
// from THIS store are now genuinely threaded through to a real backend --
// `session::join_room` (src-tauri/src/session.rs) uses them as the real
// LiveKit room-join identity/display name (see /meeting/[room]/+page.svelte's
// `join_room` call). So the *values* here are real inputs to a real system;
// only the *storage mechanism* (localStorage, not a native profile) remains
// a stand-in.
import { browser } from '$app/environment';
import { invoke } from '@tauri-apps/api/core';
import type { IdentityColor } from '$lib/components/Avatar.svelte';
import { COMMANDS, hasTauriBridge, type RemoteControlPolicy } from '$lib/ipc';
import { migrateRemoteControlPolicy } from '$lib/remoteControlPolicy';
import { STORAGE_KEYS } from '$lib/data/storageKeys';

const STORAGE_KEY = STORAGE_KEYS.onboardingSession;

interface StoredSession {
  onboardingComplete: boolean;
  name: string;
  identity: IdentityColor;
  /**
   * Stable per-install participant id, used as the real LiveKit room-join
   * identity (session::join_room's `identity` param) -- see
   * /meeting/[room]/+page.svelte's join_room call site. Generated once and
   * persisted alongside name/color; NOT a real multi-device/account
   * identity (there's no login system), just this browser/install's stable
   * handle so rejoining a room after a reload is recognizably "the same
   * participant" rather than a fresh random identity every time.
   */
  participantId: string;
  /**
   * issue #28: chosen mic/speaker device GUIDs from `list_audio_devices`.
   * Empty string means no explicit choice; use the system default.
   */
  micDeviceId: string;
  speakerDeviceId: string;
  cameraDeviceId: string;
  /** User-chosen camera capture mode (Settings resolution/FPS menus).
   * null = Auto (best healthy mode). Seeded into Rust's camera prefs on
   * launch, mirroring cameraDeviceId. */
  cameraMode: { width: number; height: number; frameRate: number } | null;
  /**
   * Global default remote-control policy for this user's shared windows,
   * seeded into Rust's meeting-scoped gate on join: `off` refuses every
   * request, `ask` (default) prompts the sharer per request (consent flow),
   * `auto` is the pre-consent behaviour (any in-room requester is granted).
   * Replaces the boolean `allowRemoteControlByDefault` (true -> ask, false
   * -> off; see `load()`), which is still read for migration only.
   */
  remoteControlPolicy: RemoteControlPolicy;
  /** @deprecated migration-only; never written. */
  allowRemoteControlByDefault?: boolean;
  /**
   * General "send diagnostics to Sentry" switch -- not panic-only (the user's
   * own framing: "we'll use it for other stuff in the future"). Gates every
   * Sentry capture path (panics, ObjC exceptions, bridged log::error!/warn!)
   * via a single Rust-side choke point (`logging::SENTRY_ENABLED`, set
   * through `set_sentry_enabled`). Default ON, same posture as
   * `allowRemoteControlByDefault`.
   */
  sentryEnabled: boolean;
  /**
   * Refs #378: opt-in, default OFF. When enabled, the controller overlay
   * (compositor/control route + web-harness equivalent) renders purely
   * local, ephemeral "input sent" feedback (Phase 1 gesture echo: click
   * ripple, keypress flash) and, for typed characters, an optimistic
   * translucent "pending" composition strip (Phase 2) that clears once the
   * real frame confirms it or after a bounded timeout. Zero wire changes --
   * this only affects what the controller renders locally for themselves.
   * Per the user decision on #378, ships default OFF; local echo is a
   * prediction, never drawn as if it were confirmed remote state (truth-
   * over-appearance).
   */
  localEchoEnabled: boolean;
}

function newParticipantId(): string {
  if (browser && 'randomUUID' in crypto) return crypto.randomUUID();
  return `p-${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

const defaults: StoredSession = {
  onboardingComplete: false,
  name: '',
  identity: 'slate',
  participantId: '',
  micDeviceId: '',
  speakerDeviceId: '',
  cameraDeviceId: '',
  cameraMode: null,
  remoteControlPolicy: 'ask',
  sentryEnabled: true,
  localEchoEnabled: false
};

function load(): StoredSession {
  if (!browser) return { ...defaults };
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return { ...defaults, participantId: newParticipantId() };
    const parsed = JSON.parse(raw);
    const merged = { ...defaults, ...parsed };
    // Backfill for sessions persisted before `participantId` existed, so an
    // existing localStorage session (from before the join-flow task) gets a
    // stable id on next load rather than an empty string being sent as a
    // LiveKit identity.
    if (!merged.participantId) merged.participantId = newParticipantId();
    merged.remoteControlPolicy = migrateRemoteControlPolicy(parsed);
    delete merged.allowRemoteControlByDefault;
    return merged;
  } catch {
    return { ...defaults, participantId: newParticipantId() };
  }
}

function persist(state: StoredSession) {
  if (!browser) return;
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
  } catch {
    // Best-effort only — this is a mock stand-in, not real persistence.
  }
}

const initial = load();

/** Svelte 5 rune-based store — frontend-only stand-in, see file header. */
export const session = $state<StoredSession>(initial);
if (browser && !initial.participantId) {
  // Defensive: `load()` above should already guarantee this, but persist
  // immediately if somehow still empty so subsequent reads (e.g. the very
  // first `join_room` call this session) see a real value.
  session.participantId = newParticipantId();
}
persist(session);

export function completeOnboarding(name: string, identity: IdentityColor) {
  session.onboardingComplete = true;
  session.name = name;
  session.identity = identity;
  persist(session);
}

export function updateIdentity(name: string, identity: IdentityColor) {
  session.name = name;
  session.identity = identity;
  persist(session);
}

export function updateAudioDevices(
  micDeviceId?: string,
  speakerDeviceId?: string,
  cameraDeviceId?: string
) {
  if (micDeviceId !== undefined) session.micDeviceId = micDeviceId;
  if (speakerDeviceId !== undefined) session.speakerDeviceId = speakerDeviceId;
  if (cameraDeviceId !== undefined) session.cameraDeviceId = cameraDeviceId;
  persist(session);
}

export function updateCameraMode(
  mode: { width: number; height: number; frameRate: number } | null
) {
  session.cameraMode = mode;
  persist(session);
}

export function updateRemoteControlPolicy(policy: RemoteControlPolicy) {
  session.remoteControlPolicy = policy;
  persist(session);
  if (browser && hasTauriBridge()) {
    // Sets BOTH the live meeting gate and the default it restores to.
    void invoke(COMMANDS.setRemoteControlPolicy, { policy }).catch(() => {});
  }
}

export function updateSentryEnabled(enabled: boolean) {
  session.sentryEnabled = enabled;
  persist(session);
  if (browser && hasTauriBridge()) {
    void invoke(COMMANDS.setSentryEnabled, { enabled }).catch(() => {});
  }
}

/**
 * Refs #378: purely a local-rendering toggle for the controller overlay --
 * no Rust/native counterpart and no wire message, so there is nothing to
 * invoke() here (unlike `updateRemoteControlDefault`/`updateSentryEnabled`,
 * which mirror state into the Rust core). The control route reads
 * `session.localEchoEnabled` directly.
 */
export function updateLocalEchoEnabled(enabled: boolean) {
  session.localEchoEnabled = enabled;
  persist(session);
}

/** Dev/debug escape hatch — not exposed in any real UI. */
export function resetOnboarding() {
  Object.assign(session, { ...defaults, participantId: newParticipantId() });
  persist(session);
}
