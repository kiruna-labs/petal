// Active-speaker spotlight controller (issue #80).
//
// Drives the speaking ring + auto-spotlight from the gallery bridge's
// `activeSpeakers` signal. Extracted verbatim from /meeting/[room]/+page.svelte
// — same promote/hold timers and thresholds, zero behavior change.
//
// - `speakingIdentities`: who currently shows the speaking ring. Held briefly
//   after LiveKit reports silence so brief pauses between words don't look
//   like the speaker vanished.
// - `activeSpeakerIdentity`: the promoted spotlight, updated only after a
//   candidate has been the top speaker continuously for SPEAKER_PROMOTE_MS.

import type { GalleryBridgeSignals } from '$lib/data/galleryBridge';

const SPEAKER_PROMOTE_MS = 1200;
const SPEAKER_RING_HOLD_MS = 1600;

export interface SpeakerSpotlight {
  readonly speakingIdentities: string[];
  readonly activeSpeakerIdentity: string | null;
  updateActiveSpeaker(speakers: GalleryBridgeSignals['activeSpeakers']): void;
  /** Reset all spotlight state + clear the hold timer (bridge teardown). */
  reset(): void;
  /** Tear down the promotion timer (call from onDestroy). */
  dispose(): void;
}

export function createSpeakerSpotlight(): SpeakerSpotlight {
  let speakingIdentities = $state<string[]>([]);
  let activeSpeakerIdentity = $state<string | null>(null);
  let speakerCandidateIdentity: string | null = null;
  let speakerPromoteTimer: ReturnType<typeof setTimeout> | undefined;
  let speakerRingHoldTimer: ReturnType<typeof setTimeout> | undefined;

  function clearSpeakerPromotion() {
    if (speakerPromoteTimer) clearTimeout(speakerPromoteTimer);
    speakerPromoteTimer = undefined;
    speakerCandidateIdentity = null;
  }

  function promoteSpeaker(identity: string) {
    activeSpeakerIdentity = identity;
    clearSpeakerPromotion();
  }

  function updateActiveSpeaker(speakers: GalleryBridgeSignals['activeSpeakers']) {
    const identities = speakers.map((speaker) => speaker.identity);
    if (identities.length > 0) {
      if (speakerRingHoldTimer) clearTimeout(speakerRingHoldTimer);
      speakingIdentities = identities;
    } else {
      // Keep the ring around briefly after LiveKit reports silence; brief
      // pauses between words should not look like the speaker vanished.
      if (speakerRingHoldTimer) clearTimeout(speakerRingHoldTimer);
      speakerRingHoldTimer = setTimeout(() => {
        speakingIdentities = [];
      }, SPEAKER_RING_HOLD_MS);
      clearSpeakerPromotion();
      return;
    }

    const top = speakers[0];
    if (!top || top.identity === activeSpeakerIdentity) {
      clearSpeakerPromotion();
      return;
    }
    if (top.identity === speakerCandidateIdentity) return;

    clearSpeakerPromotion();
    speakerCandidateIdentity = top.identity;
    speakerPromoteTimer = setTimeout(() => {
      if (speakerCandidateIdentity === top.identity) promoteSpeaker(top.identity);
    }, SPEAKER_PROMOTE_MS);
  }

  return {
    get speakingIdentities() {
      return speakingIdentities;
    },
    get activeSpeakerIdentity() {
      return activeSpeakerIdentity;
    },
    updateActiveSpeaker,
    reset() {
      speakingIdentities = [];
      activeSpeakerIdentity = null;
      clearSpeakerPromotion();
      if (speakerRingHoldTimer) clearTimeout(speakerRingHoldTimer);
      speakerRingHoldTimer = undefined;
    },
    dispose() {
      clearSpeakerPromotion();
      if (speakerRingHoldTimer) clearTimeout(speakerRingHoldTimer);
      speakerRingHoldTimer = undefined;
    }
  };
}
