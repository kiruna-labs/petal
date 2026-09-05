import {
  HARNESS_FAVORITES_STORAGE_KEY,
  HARNESS_COLOR_STORAGE_KEY,
  HARNESS_NAME_STORAGE_KEY,
  HARNESS_RECENTS_STORAGE_KEY,
  HARNESS_ROOM_STORAGE_KEY,
  MAX_RECENT_ROOMS,
} from './constants.ts';
import { inviteLinkForCredential } from './controls.ts';
import { inviteLinkCopiedToastMessage } from './inviteToast.ts';
import { looksLikeJoinAttempt, parseJoinInput } from '@petal/shared/logic/joinInput';
import { accessCodeForCredential, registerAccessCodeForCredential } from '@petal/shared/logic/meetingCode';
import { roomDisplayLabelForCredential } from './roomLabels.ts';
import { installDismissibleLayer } from '@petal/shared/ui/dismissibleLayer';
import { colorForIdentity, IDENTITY_COLOR_PALETTE } from './telepointer.ts';
import type { LogKind } from './ui/logging.ts';

interface RecentRoom {
  code: string;
  lastJoinedAt: number;
  joinCount: number;
  /** Best-effort short access code, persisted so a rejoin in a later page
   * load (which never re-derives it) can still build a real invite link
   * instead of silently degrading to the bare origin. Absent for older
   * stored records or when the code wasn't resolvable at record time. */
  accessCode?: string;
}

interface HomeScreenOptions {
  joinCard: HTMLDivElement;
  displayNameInput: HTMLInputElement;
  profileAvatarInitial: HTMLElement | null;
  profileAvatarButton?: HTMLButtonElement | null;
  profileColorBubble?: HTMLButtonElement | null;
  profileColorSwatches?: HTMLButtonElement[];
  profileOnboarding?: HTMLElement | null;
  profileOnboardingDone?: HTMLButtonElement | null;
  showFirstVisitOnboarding?: boolean;
  meetingCodeInput: HTMLInputElement;
  joinBtn: HTMLButtonElement;
  createBtn?: HTMLButtonElement | null;
  connError: HTMLElement;
  joinHint: HTMLElement;
  submitMeetingField: () => Promise<void>;
  showToast?: (message: string) => void;
  logEvent?: (message: string, kind?: LogKind) => void;
}

export interface HomeScreenApi {
  updateUnifiedCtaLabel: () => void;
  recordRecentRoom: (code: string) => void;
  refreshRecentRooms: () => void;
  roomDisplayLabelForCredential: (code: string) => string;
}

interface RecentRoomInviteCopyOptions {
  credential: string;
  displayLabel: string;
  origin?: string;
  clipboard?: Pick<Clipboard, 'writeText'>;
  showToast?: (message: string) => void;
  logEvent?: (message: string, kind?: LogKind) => void;
}

export async function copyRecentRoomInviteLink({
  credential,
  displayLabel,
  origin = location.origin,
  clipboard = navigator.clipboard,
  showToast,
  logEvent,
}: RecentRoomInviteCopyOptions): Promise<string> {
  const url = inviteLinkForCredential(credential, origin, displayLabel);
  try {
    await clipboard.writeText(url);
    showToast?.(inviteLinkCopiedToastMessage(url));
    logEvent?.(`invite link copied: ${url}`, 'ok');
  } catch {
    showToast?.(inviteLinkCopiedToastMessage(url));
    logEvent?.(`clipboard unavailable -- invite link: ${url}`, 'warn');
  }
  return url;
}

export function parseStoredColorIndex(raw: string | null): number | null {
  if (raw === null) return null;
  const index = Number(raw);
  return Number.isInteger(index) && index >= 0 && index < IDENTITY_COLOR_PALETTE.length ? index : null;
}

export function loadStoredColorIndex(storage: Pick<Storage, 'getItem'>): number | null {
  return parseStoredColorIndex(storage.getItem(HARNESS_COLOR_STORAGE_KEY));
}

export function saveStoredColorIndex(storage: Pick<Storage, 'setItem'>, index: number): void {
  if (!Number.isInteger(index) || index < 0 || index >= IDENTITY_COLOR_PALETTE.length) return;
  storage.setItem(HARNESS_COLOR_STORAGE_KEY, String(index));
}

export function ensureStoredColorIndex(
  storage: Pick<Storage, 'getItem' | 'setItem'>,
  paletteLength: number = IDENTITY_COLOR_PALETTE.length,
  rng: () => number = Math.random,
): number {
  const stored = loadStoredColorIndex(storage);
  if (stored !== null) return stored;
  const index = Math.floor(rng() * paletteLength);
  saveStoredColorIndex(storage, index);
  return index;
}

/** Keyboard movement for the compact two-column profile color palette. */
export function nextProfileColorIndex(
  currentIndex: number,
  key: string,
  paletteLength: number = IDENTITY_COLOR_PALETTE.length,
): number | null {
  if (paletteLength <= 0) return null;
  const current = Math.min(Math.max(currentIndex, 0), paletteLength - 1);
  switch (key) {
    case 'ArrowRight': return (current + 1) % paletteLength;
    case 'ArrowLeft': return (current - 1 + paletteLength) % paletteLength;
    case 'ArrowDown': return (current + 2) % paletteLength;
    case 'ArrowUp': return (current - 2 + paletteLength) % paletteLength;
    case 'Home': return 0;
    case 'End': return paletteLength - 1;
    default: return null;
  }
}

export function setupHomeScreen(options: HomeScreenOptions): HomeScreenApi {
  const {
    joinCard,
    displayNameInput,
    profileAvatarInitial,
    profileAvatarButton,
    profileColorBubble,
    profileColorSwatches = [],
    profileOnboarding,
    profileOnboardingDone,
    showFirstVisitOnboarding = false,
    meetingCodeInput,
    joinBtn,
    connError,
    joinHint,
    submitMeetingField,
    showToast,
    logEvent,
  } = options;
  let selectedColorIndex = ensureStoredColorIndex(localStorage);

  const recentRoomsEl = document.createElement('div');
  recentRoomsEl.className = 'recent-rooms hidden';
  const homeBody = connError.parentElement ?? joinCard;
  homeBody.insertBefore(recentRoomsEl, connError);

  function safeJsonArray(raw: string | null): unknown[] {
    if (!raw) return [];
    try {
      const parsed = JSON.parse(raw);
      return Array.isArray(parsed) ? parsed : [];
    } catch {
      return [];
    }
  }

  function normalizeRecentCode(code: string): string {
    return code.trim().toLowerCase();
  }

  function displayLabelForCredential(code: string): string {
    return roomDisplayLabelForCredential(code);
  }

  function setPendingRecentCredential(code: string): boolean {
    const normalized = normalizeRecentCode(code);
    if (!normalized) {
      meetingCodeInput.value = '';
      delete meetingCodeInput.dataset.petalRoomCredential;
      delete meetingCodeInput.dataset.petalRoomDisplayLabel;
      return false;
    }
    const label = displayLabelForCredential(normalized);
    meetingCodeInput.value = label;
    meetingCodeInput.dataset.petalRoomCredential = normalized;
    meetingCodeInput.dataset.petalRoomDisplayLabel = label;
    return true;
  }

  function pendingRecentCredential(): string | null {
    const credential = meetingCodeInput.dataset.petalRoomCredential;
    if (!credential) return null;
    const label = meetingCodeInput.dataset.petalRoomDisplayLabel;
    if (label && meetingCodeInput.value.trim() === label) return credential;
    delete meetingCodeInput.dataset.petalRoomCredential;
    delete meetingCodeInput.dataset.petalRoomDisplayLabel;
    return null;
  }

  function loadRecentRooms(): RecentRoom[] {
    const byCode = new Map<string, RecentRoom>();
    safeJsonArray(localStorage.getItem(HARNESS_RECENTS_STORAGE_KEY)).forEach((item) => {
      if (!item || typeof item !== 'object') return;
      const record = item as Partial<RecentRoom>;
      if (typeof record.code !== 'string') return;
      const code = normalizeRecentCode(record.code);
      if (!code) return;
      if (typeof record.accessCode === 'string') {
        registerAccessCodeForCredential(code, record.accessCode);
      }
      byCode.set(code, {
        code,
        lastJoinedAt: typeof record.lastJoinedAt === 'number' ? record.lastJoinedAt : 0,
        joinCount: typeof record.joinCount === 'number' ? Math.max(1, record.joinCount) : 1,
        accessCode: typeof record.accessCode === 'string' ? record.accessCode : undefined,
      });
    });

    const legacyLastRoom = normalizeRecentCode(localStorage.getItem(HARNESS_ROOM_STORAGE_KEY) ?? '');
    if (legacyLastRoom && !byCode.has(legacyLastRoom)) {
      byCode.set(legacyLastRoom, { code: legacyLastRoom, lastJoinedAt: 0, joinCount: 1 });
    }

    return Array.from(byCode.values());
  }

  function saveRecentRooms(rooms: RecentRoom[]) {
    localStorage.setItem(HARNESS_RECENTS_STORAGE_KEY, JSON.stringify(rooms.slice(0, MAX_RECENT_ROOMS)));
  }

  function loadFavoriteRooms(): Set<string> {
    const favorites = new Set<string>();
    safeJsonArray(localStorage.getItem(HARNESS_FAVORITES_STORAGE_KEY)).forEach((item) => {
      if (typeof item !== 'string') return;
      const code = normalizeRecentCode(item);
      if (code) favorites.add(code);
    });
    return favorites;
  }

  function saveFavoriteRooms(favorites: Set<string>) {
    localStorage.setItem(HARNESS_FAVORITES_STORAGE_KEY, JSON.stringify(Array.from(favorites).sort()));
  }

  function sortedRecentRooms(): RecentRoom[] {
    const favorites = loadFavoriteRooms();
    return loadRecentRooms().sort((a, b) => {
      const favoriteDelta = Number(favorites.has(b.code)) - Number(favorites.has(a.code));
      if (favoriteDelta !== 0) return favoriteDelta;
      if (b.lastJoinedAt !== a.lastJoinedAt) return b.lastJoinedAt - a.lastJoinedAt;
      return a.code.localeCompare(b.code);
    });
  }

  function formatRecentTime(lastJoinedAt: number): string {
    if (lastJoinedAt <= 0) return 'recent';
    const seconds = Math.max(1, Math.floor((Date.now() - lastJoinedAt) / 1000));
    if (seconds < 60) return 'just now';
    const minutes = Math.floor(seconds / 60);
    if (minutes < 60) return `${minutes}m ago`;
    const hours = Math.floor(minutes / 60);
    if (hours < 24) return `${hours}h ago`;
    const days = Math.floor(hours / 24);
    return `${days}d ago`;
  }

  function starIconSvg(): string {
    return [
      '<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor"',
      'stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">',
      '<polygon points="12 2 15.1 8.3 22 9.3 17 14.2 18.2 21 12 17.8 5.8 21 7 14.2 2 9.3 8.9 8.3 12 2"></polygon>',
      '</svg>',
    ].join(' ');
  }

  function linkIconSvg(): string {
    return [
      '<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor"',
      'stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">',
      '<path d="M10 13a5 5 0 0 0 7.07 0l2.12-2.12a5 5 0 0 0-7.07-7.07L10.9 5.03"></path>',
      '<path d="M14 11a5 5 0 0 0-7.07 0L4.81 13.12a5 5 0 0 0 7.07 7.07l1.22-1.22"></path>',
      '</svg>',
    ].join(' ');
  }

  function renderRecentRooms() {
    const rooms = sortedRecentRooms();
    const favorites = loadFavoriteRooms();
    recentRoomsEl.replaceChildren();
    recentRoomsEl.classList.toggle('hidden', rooms.length === 0);
    if (rooms.length === 0) return;

    const header = document.createElement('div');
    header.className = 'recent-rooms__header';
    header.textContent = 'YOUR ROOMS';
    recentRoomsEl.appendChild(header);

    rooms.forEach((roomRecord) => {
      const favorite = favorites.has(roomRecord.code);
      const roomLabel = displayLabelForCredential(roomRecord.code);
      const row = document.createElement('div');
      row.className = 'recent-room';
      row.classList.toggle('is-favorite', favorite);

      const star = document.createElement('button');
      star.type = 'button';
      star.className = 'recent-room__star';
      star.classList.toggle('is-favorite', favorite);
      star.innerHTML = starIconSvg();
      star.setAttribute('aria-label', favorite ? `Remove ${roomLabel} from favorites` : `Favorite ${roomLabel}`);
      star.title = favorite ? 'Unfavorite' : 'Favorite';
      star.addEventListener('click', (event) => {
        event.preventDefault();
        event.stopPropagation();
        const nextFavorites = loadFavoriteRooms();
        if (nextFavorites.has(roomRecord.code)) {
          nextFavorites.delete(roomRecord.code);
        } else {
          nextFavorites.add(roomRecord.code);
        }
        saveFavoriteRooms(nextFavorites);
        renderRecentRooms();
      });

      let copyTimer: number | undefined;
      const copy = document.createElement('button');
      copy.type = 'button';
      copy.className = 'recent-room__copy';
      copy.innerHTML = linkIconSvg();
      const roomId = roomRecord.accessCode ?? accessCodeForCredential(roomRecord.code);
      const roomIdLabel = roomId ?? 'not available';
      copy.setAttribute('aria-label', `Room ID ${roomIdLabel}, click to copy invite`);
      copy.title = `Room ID: ${roomIdLabel} (click to copy invite)`;
      copy.addEventListener('click', (event) => {
        event.preventDefault();
        event.stopPropagation();
        window.clearTimeout(copyTimer);
        copy.classList.add('copied');
        copy.textContent = 'Copied';
        void copyRecentRoomInviteLink({
          credential: roomRecord.code,
          displayLabel: roomLabel,
          showToast,
          logEvent,
        });
        copyTimer = window.setTimeout(() => {
          copy.classList.remove('copied');
          copy.innerHTML = linkIconSvg();
        }, 1400);
      });

      const roomButton = document.createElement('button');
      roomButton.type = 'button';
      roomButton.className = 'recent-room__button';
      roomButton.addEventListener('click', () => {
        if (!setPendingRecentCredential(roomRecord.code)) {
          meetingCodeInput.value = roomLabel;
          delete meetingCodeInput.dataset.petalRoomCredential;
          delete meetingCodeInput.dataset.petalRoomDisplayLabel;
        }
        updateUnifiedCtaLabel();
        void submitMeetingField();
      });

      const code = document.createElement('span');
      code.className = 'recent-room__code';
      code.textContent = roomLabel;
      const meta = document.createElement('span');
      meta.className = 'recent-room__meta';
      meta.textContent = favorite ? `favorite, ${formatRecentTime(roomRecord.lastJoinedAt)}` : formatRecentTime(roomRecord.lastJoinedAt);
      roomButton.append(code, meta);
      row.append(roomButton, copy, star);
      recentRoomsEl.appendChild(row);
    });
  }

  function updateProfileAvatarInitial() {
    if (!profileAvatarInitial) return;
    const initial = Array.from(displayNameInput.value.trim())[0]?.toLocaleUpperCase() ?? '';
    profileAvatarInitial.textContent = initial;
    profileAvatarInitial.parentElement?.classList.toggle('is-empty', !initial);
    profileAvatarInitial.parentElement?.style.setProperty(
      'color',
      colorForIdentity(displayNameInput.value.trim() || 'Guest', selectedColorIndex)
    );
  }

  function updateProfileColorPicker() {
    const color = colorForIdentity('', selectedColorIndex);
    const colorName = profileColorSwatches[selectedColorIndex]?.dataset.colorName ?? `color ${selectedColorIndex + 1}`;
    profileColorBubble?.style.setProperty('--swatch-color', color);
    profileColorBubble?.setAttribute('aria-label', `Change profile color, currently ${colorName}`);
    profileColorSwatches.forEach((swatch, index) => {
      swatch.style.setProperty('--swatch-color', colorForIdentity('', index));
      swatch.classList.toggle('selected', selectedColorIndex === index);
      swatch.setAttribute('aria-pressed', String(selectedColorIndex === index));
    });
    updateProfileAvatarInitial();
  }

  function closeProfileColorPopover(restoreFocus = false) {
    const popover = profileColorBubble?.parentElement?.querySelector<HTMLElement>('.profile-color-options');
    if (!popover) return;
    const wasOpen = !popover.hidden;
    popover.hidden = true;
    profileColorBubble?.setAttribute('aria-expanded', 'false');
    if (wasOpen && restoreFocus) profileColorBubble?.focus();
  }

  function openProfileColorPopover() {
    const popover = profileColorBubble?.parentElement?.querySelector<HTMLElement>('.profile-color-options');
    if (!popover || !profileColorBubble) return;
    popover.hidden = false;
    profileColorBubble.setAttribute('aria-expanded', 'true');
    profileColorSwatches[selectedColorIndex]?.focus();
  }

  function hideProfileOnboarding() {
    closeProfileColorPopover();
    profileOnboarding?.classList.add('hidden');
    profileAvatarButton?.setAttribute('aria-expanded', 'false');
  }

  function openProfileOnboarding() {
    if (!profileOnboarding) return;
    profileOnboarding.classList.remove('hidden');
    profileAvatarButton?.setAttribute('aria-expanded', 'true');
    displayNameInput.focus();
  }

  function updateProfileSaveState() {
    if (profileOnboardingDone) profileOnboardingDone.disabled = !displayNameInput.value.trim();
  }

  function showProfileOnboarding() {
    if (!showFirstVisitOnboarding) return;
    openProfileOnboarding();
  }

  function toggleProfileOnboarding() {
    if (!profileOnboarding) return;
    if (profileOnboarding.classList.contains('hidden')) {
      openProfileOnboarding();
    } else {
      hideProfileOnboarding();
    }
  }

  function updateUnifiedCtaLabel() {
    const value = meetingCodeInput.value.trim();
    if (!value) {
      joinBtn.textContent = 'Create/Join';
    } else {
      joinBtn.textContent = pendingRecentCredential() || parseJoinInput(value).ok || looksLikeJoinAttempt(value) ? 'Join' : 'Create';
    }
  }

  function recordRecentRoom(code: string) {
    const normalized = normalizeRecentCode(code);
    if (!normalized) return;
    const existing = loadRecentRooms();
    const previous = existing.find((room) => room.code === normalized);
    // Best-effort: only resolvable if this page load itself generated or
    // parsed the code (see registerAccessCodeForCredential's doc comment).
    // Persisting it here is what lets a LATER page load's rejoin still work.
    const accessCode = accessCodeForCredential(normalized) ?? previous?.accessCode;
    const updated: RecentRoom = {
      code: normalized,
      lastJoinedAt: Date.now(),
      joinCount: (previous?.joinCount ?? 0) + 1,
      accessCode: accessCode ?? undefined,
    };
    const next = [updated, ...existing.filter((room) => room.code !== normalized)].sort(
      (a, b) => b.lastJoinedAt - a.lastJoinedAt
    );
    saveRecentRooms(next);
    renderRecentRooms();
  }

  function isLocalhostPreview(): boolean {
    return location.hostname === 'localhost' || location.hostname === '127.0.0.1' || location.hostname === '::1';
  }

  function installDesktopPrototypeLink() {
    if (!isLocalhostPreview()) return;
    const link = document.createElement('a');
    link.className = 'desktop-prototype-link';
    link.href = 'http://localhost:1420/dev/main-menu';
    link.target = '_blank';
    link.rel = 'noreferrer';
    link.textContent = 'Open desktop prototype';
    const parent = joinHint.parentElement ?? homeBody;
    parent.insertBefore(link, joinHint.nextSibling);
  }

  displayNameInput.addEventListener('input', () => {
    updateProfileAvatarInitial();
    updateProfileSaveState();
  });
  displayNameInput.addEventListener('change', () => {
    const name = displayNameInput.value.trim();
    if (name) localStorage.setItem(HARNESS_NAME_STORAGE_KEY, name);
    updateProfileAvatarInitial();
  });
  profileColorSwatches.forEach((swatch) => {
    swatch.addEventListener('click', () => {
      const index = parseStoredColorIndex(swatch.dataset.colorIndex ?? null);
      if (index === null) return;
      selectedColorIndex = index;
      saveStoredColorIndex(localStorage, index);
      updateProfileColorPicker();
      closeProfileColorPopover(true);
    });
  });
  profileColorBubble?.addEventListener('click', () => {
    const popover = profileColorBubble.parentElement?.querySelector<HTMLElement>('.profile-color-options');
    if (popover?.hidden) openProfileColorPopover();
    else closeProfileColorPopover();
  });
  profileColorBubble?.parentElement?.querySelector<HTMLElement>('.profile-color-options')?.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') {
      event.preventDefault();
      closeProfileColorPopover(true);
      return;
    }
    const activeIndex = profileColorSwatches.indexOf(document.activeElement as HTMLButtonElement);
    const nextIndex = nextProfileColorIndex(activeIndex < 0 ? selectedColorIndex : activeIndex, event.key);
    if (nextIndex === null) return;
    event.preventDefault();
    profileColorSwatches[nextIndex]?.focus();
  });
  installDismissibleLayer({
    isOpen: () => {
      const popover = profileColorBubble?.parentElement?.querySelector<HTMLElement>('.profile-color-options');
      return !!popover && !popover.hidden;
    },
    getInsideNodes: () => [profileColorBubble?.parentElement],
    getPopupNodes: () => [profileColorBubble?.parentElement?.querySelector<HTMLElement>('.profile-color-options')],
    getOpener: () => profileColorBubble,
    onDismiss: () => closeProfileColorPopover(false)
  });
  profileOnboardingDone?.addEventListener('click', () => {
    if (!displayNameInput.value.trim()) return;
    hideProfileOnboarding();
  });
  profileAvatarButton?.addEventListener('click', toggleProfileOnboarding);
  meetingCodeInput.addEventListener('input', () => {
    delete meetingCodeInput.dataset.petalRoomCredential;
    delete meetingCodeInput.dataset.petalRoomDisplayLabel;
    updateUnifiedCtaLabel();
  });
  meetingCodeInput.addEventListener('keydown', (event) => {
    if (event.key !== 'Enter') return;
    event.preventDefault();
    void submitMeetingField();
  });

  installDesktopPrototypeLink();
  updateProfileColorPicker();
  updateProfileSaveState();
  renderRecentRooms();
  setPendingRecentCredential(meetingCodeInput.value);
  updateUnifiedCtaLabel();
  showProfileOnboarding();

  return {
    updateUnifiedCtaLabel,
    recordRecentRoom,
    refreshRecentRooms: renderRecentRooms,
    roomDisplayLabelForCredential: displayLabelForCredential,
  };
}
