// SINGLE SOURCE OF TRUTH for tile layout-mode transitions and for choosing a
// spotlight hero (#785). Shared by the web client (web-harness/src/
// tileLayout.ts + tiles.ts + connection.ts) and the desktop gallery
// (apps/desktop/src/lib/components/Gallery.svelte) so the two surfaces cannot
// drift on the two rules this module exists to enforce:
//
//  1. An AUTOMATIC transition (a share arriving auto-spotlights) never writes
//     the user's persisted layout preference, and records the mode it left so
//     the client can return to it when the share ends. Only an explicit user
//     action may change the stored preference.
//  2. The local participant's own self-view is never auto-promoted to hero
//     while any remote candidate exists. Staring at your own webcam is never a
//     useful default -- it is the exact symptom #785 was filed for.
//
// Pure: no DOM, no storage, no framework. Callers adapt their own state into
// `TileLayoutModeState` / `SpotlightCandidate` and apply the returned
// transition themselves (the web client writes localStorage only when
// `persist` is non-null; the desktop gallery persists nothing at all).

export type TileLayoutMode = 'grid' | 'spotlight';

export interface TileLayoutModeState {
  /** The layout the client is showing right now. */
  readonly mode: TileLayoutMode;
  /**
   * The mode to return to when an automatic spotlight ends. `null` means
   * there is nothing to restore: either no automatic switch is in effect, or
   * the user has since made an explicit choice that superseded it.
   */
  readonly restoreMode: TileLayoutMode | null;
}

export interface TileLayoutModeTransition {
  readonly state: TileLayoutModeState;
  /**
   * The mode the caller must write to persistent storage, or `null` when the
   * transition must not touch the stored preference at all. Never conflate
   * this with `state.mode` -- an automatic switch changes `state.mode` while
   * leaving `persist` null, and that difference IS the fix for #785's third
   * defect (an auto-spotlight outliving the session that caused it).
   */
  readonly persist: TileLayoutMode | null;
}

export function initialTileLayoutModeState(mode: TileLayoutMode): TileLayoutModeState {
  return { mode, restoreMode: null };
}

/**
 * The client auto-spotlighted something (first share arriving). Records the
 * mode being left so it can be restored, and never persists.
 *
 * Idempotent while already spotlighted: a second share must not overwrite the
 * restore slot with 'spotlight' and strand the user there.
 */
export function autoSpotlight(state: TileLayoutModeState): TileLayoutModeTransition {
  if (state.mode === 'spotlight') return { state, persist: null };
  return { state: { mode: 'spotlight', restoreMode: state.mode }, persist: null };
}

/**
 * An explicit user action selected `mode` (layout picker, or a click that pins
 * a tile). Persists, and DISCARDS any recorded restore state: once the user
 * has chosen for themselves, a later automatic restore must not undo it.
 */
export function manualTileLayoutMode(
  _state: TileLayoutModeState,
  mode: TileLayoutMode
): TileLayoutModeTransition {
  return { state: { mode, restoreMode: null }, persist: mode };
}

/**
 * The condition that triggered an automatic spotlight is gone (the last share
 * ended, or the session did). Returns to the recorded mode without persisting;
 * a no-op when nothing was recorded, which is what keeps a user who chose
 * spotlight themselves in spotlight.
 */
export function endAutoSpotlight(state: TileLayoutModeState): TileLayoutModeTransition {
  if (state.restoreMode === null) return { state, persist: null };
  return { state: { mode: state.restoreMode, restoreMode: null }, persist: null };
}

/**
 * The user dismissed the spotlighted surface itself (web's remote-window
 * "minimize"). Prefers the recorded restore over an unconditional 'grid', so
 * dismissing an auto-spotlight lands where the user actually was; with nothing
 * recorded it is an ordinary explicit switch to grid.
 */
export function dismissSpotlight(state: TileLayoutModeState): TileLayoutModeTransition {
  if (state.restoreMode !== null) return endAutoSpotlight(state);
  return manualTileLayoutMode(state, 'grid');
}

export interface SpotlightCandidate {
  /** Stable identifier: a DOM element id on web, a participant key natively. */
  readonly key: string;
  /** This candidate renders shared window content itself (web share tile). */
  readonly isShare?: boolean;
  /**
   * The participant behind this candidate is sharing, but the candidate is
   * their camera tile rather than the share (native: shares are compositor
   * NSWindows, never gallery tiles).
   */
  readonly isSharing?: boolean;
  /** Smoothed active speaker. */
  readonly isActiveSpeaker?: boolean;
  /** Has live video to show. */
  readonly hasVideo?: boolean;
  /** The local participant's own tile. */
  readonly isLocal?: boolean;
}

/**
 * Lower ranks win. The whole point of the ordering is the last line: a local
 * candidate that is not itself a share sits BELOW every remote candidate,
 * including a remote tile with no video at all. `isSharing` deliberately does
 * not rescue a local candidate -- natively, "I am sharing" promotes my own
 * webcam, which is the self-view symptom, not the share.
 */
const SPOTLIGHT_RANK = {
  remoteShare: 0,
  remoteSharingParticipant: 1,
  localShare: 2,
  remoteActiveSpeaker: 3,
  remoteWithVideo: 4,
  remote: 5,
  localSelfView: 6,
} as const;

export function spotlightCandidateRank(candidate: SpotlightCandidate): number {
  if (candidate.isShare) {
    return candidate.isLocal ? SPOTLIGHT_RANK.localShare : SPOTLIGHT_RANK.remoteShare;
  }
  if (candidate.isLocal) return SPOTLIGHT_RANK.localSelfView;
  if (candidate.isSharing) return SPOTLIGHT_RANK.remoteSharingParticipant;
  if (candidate.isActiveSpeaker) return SPOTLIGHT_RANK.remoteActiveSpeaker;
  if (candidate.hasVideo) return SPOTLIGHT_RANK.remoteWithVideo;
  return SPOTLIGHT_RANK.remote;
}

/**
 * Picks the hero for spotlight when the user has not pinned one. Stable: ties
 * keep the caller's order, so an all-equal list yields its first entry (the
 * behaviour both clients had before this module existed).
 */
export function chooseSpotlightHero<T extends SpotlightCandidate>(candidates: readonly T[]): T | null {
  let best: T | null = null;
  let bestRank = Number.POSITIVE_INFINITY;
  for (const candidate of candidates) {
    const rank = spotlightCandidateRank(candidate);
    if (rank < bestRank) {
      best = candidate;
      bestRank = rank;
    }
  }
  return best;
}
