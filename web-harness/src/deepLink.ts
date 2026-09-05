import { HARNESS_NAME_STORAGE_KEY } from './constants.ts';
import { parseJoinInput } from '@petal/shared/logic/joinInput';
import { accessCodeForCredential } from '@petal/shared/logic/meetingCode';

interface AutoJoinOptions {
  displayNameInput: HTMLInputElement;
  meetingCodeInput: HTMLInputElement;
  joinHint: HTMLElement;
  logEvent: (message: string) => void;
  connectToMeeting: (meetingCode: string, identity: string) => Promise<void>;
  resolveIdentity: () => string;
  showError: (message: string) => void;
  updateUnifiedCtaLabel: () => void;
  /** Swap the home screen for the "Joining <label>" interstitial the moment
   * an auto-join starts, so a join link never flashes the main menu. Optional
   * so partially-wired callers (tests) keep the pre-interstitial behavior. */
  showConnectingScreen?: (label: string) => void;
}

export function autoJoinFromUrl({
  displayNameInput,
  meetingCodeInput,
  joinHint,
  logEvent,
  connectToMeeting,
  resolveIdentity,
  showError,
  updateUnifiedCtaLabel,
  showConnectingScreen,
}: AutoJoinOptions) {
  const parsed = parseJoinInput(location.href);
  const isJoinLink =
    parsed.ok ||
    new URLSearchParams(location.search).has('code') ||
    location.hash.startsWith('#/join/') ||
    location.pathname.split('/').filter(Boolean).length > 0;
  if (!isJoinLink) return;
  if (parsed.ok) {
    // `parseJoinInput` resolves a public access code to the internal
    // room-<hex> credential used by LiveKit. Keep that credential private:
    // the join field is user-visible and must continue to show the canonical
    // access code from the invite URL, even when it replaces a legacy value
    // restored from localStorage.
    const visibleAccessCode = accessCodeForCredential(parsed.code) ?? 'Petal meeting';
    meetingCodeInput.value = visibleAccessCode;
    updateUnifiedCtaLabel();
    const storedName = localStorage.getItem(HARNESS_NAME_STORAGE_KEY)?.trim();
    const visibleName = displayNameInput.value.trim();
    if (!storedName && !visibleName) {
      joinHint.textContent = 'Enter your name to join this invite.';
      joinHint.classList.remove('hidden');
      displayNameInput.focus();
      logEvent(`invite URL loaded "${parsed.code}" -- waiting for display name`);
      return;
    }
    logEvent(`auto-joining "${parsed.code}" from invite URL`);
    // The name is known, so the user has nothing to do on the menu -- go
    // straight to a joining view instead of flashing the home screen while
    // the token fetch + connect run (user request, 2026-08-11). Shows the
    // public access code, never the internal credential.
    showConnectingScreen?.(visibleAccessCode);
    void connectToMeeting(parsed.code, resolveIdentity());
  } else {
    showError(`Invite link problem: ${parsed.error}`);
  }
}
