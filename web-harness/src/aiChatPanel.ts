// The in-tile AI chat surface (#657): disclosure badge, countdown, floor
// state, hold-to-talk, error copy, and the coalesced transcript.
//
// It lives on the TILE rather than inside `remote-window-header`, because the
// header auto-hides after ~1.8s idle and the session-visibility rule requires
// that it stay unmistakable, for as long as the session runs, that this
// window's content and the room's voice are going to a third-party API.
//
// UI text must never truncate: every string here wraps (`overflow-wrap:
// anywhere` + `white-space: pre-wrap` in style.css) rather than ellipsizing,
// including transcript text and participant display names.

import {
  AI_CHAT_ACTIVE_DISCLOSURE,
  aiChatEndReasonMessage,
  formatAiChatCountdown,
  isNormalAiChatEnd,
  type AiChatSessionState,
} from './aiChat.ts';
import { AI_CHAT_TEXT_MAX_CHARS } from './trackNames.ts';
import { sparkleIconSvg } from '@petal/shared/ui/icons';

export interface AiChatPanelOptions {
  tile: HTMLElement;
  windowId: number;
  ownerIdentity: string;
  /** Resolve a LiveKit identity to a display name; falls back to the identity. */
  displayNameFor?: (identity: string) => string;
  localIdentity?: string | null;
  onStop: () => void;
  onPttStart: () => void;
  onPttEnd: () => void;
  /** Type a message into the session. Never touches the floor -- unlike PTT,
   * any participant may send one independent of who (if anyone) holds it,
   * though the panel still disables Send while someone is actively talking
   * (see the `floorTaken` check below): the Rust side refuses a typed turn
   * that would overlap an open manual-activity window regardless. */
  onSendText: (text: string) => void;
}

export interface AiChatPanelController {
  update: (
    options: AiChatPanelOptions,
    session: AiChatSessionState | null,
    localPttHeld: boolean,
  ) => void;
  destroy: () => void;
}

function appendElement<K extends keyof HTMLElementTagNameMap>(
  parent: HTMLElement,
  tagName: K,
  className: string,
): HTMLElementTagNameMap[K] {
  const element = document.createElement(tagName);
  element.className = className;
  parent.appendChild(element);
  return element;
}

function clearElement(element: HTMLElement) {
  while (element.firstChild) element.firstChild.remove();
}

export function createAiChatPanel(options: AiChatPanelOptions): AiChatPanelController {
  let current = options;

  function nameFor(identity: string | null | undefined): string {
    if (!identity) return 'someone';
    const resolved = current.displayNameFor?.(identity)?.trim();
    return resolved || identity;
  }

  const root = document.createElement('div');
  root.className = 'ai-chat-panel';
  root.setAttribute('role', 'region');
  root.setAttribute('aria-label', 'AI chat');

  const header = appendElement(root, 'div', 'ai-chat-panel__header');
  const badge = appendElement(header, 'span', 'ai-chat-panel__badge');
  badge.setAttribute('role', 'status');
  // #847: sparkle icon, persistent -- mirrors desktop's
  // RemoteWindowHeader.svelte `.ai-chat-badge-dot` (same icon, same pulse).
  // A separate child so `update()` below can replace just the text without
  // re-inserting the icon's markup on every call.
  const badgeIcon = appendElement(badge, 'span', 'ai-chat-panel__badge-icon');
  badgeIcon.setAttribute('aria-hidden', 'true');
  badgeIcon.innerHTML = sparkleIconSvg(10);
  const badgeText = appendElement(badge, 'span', 'ai-chat-panel__badge-text');
  const countdown = appendElement(header, 'span', 'ai-chat-panel__countdown');
  const stopButton = appendElement(header, 'button', 'ai-chat-panel__stop');
  stopButton.type = 'button';
  stopButton.textContent = 'Stop';
  stopButton.setAttribute('aria-label', 'Stop AI chat');

  const disclosure = appendElement(root, 'p', 'ai-chat-panel__disclosure');
  disclosure.textContent = AI_CHAT_ACTIVE_DISCLOSURE;

  const statusLine = appendElement(root, 'p', 'ai-chat-panel__status');
  statusLine.setAttribute('role', 'status');

  const transcript = appendElement(root, 'div', 'ai-chat-panel__transcript');
  transcript.setAttribute('aria-live', 'polite');

  const pttButton = appendElement(root, 'button', 'ai-chat-panel__ptt');
  pttButton.type = 'button';
  const pttLabel = appendElement(pttButton, 'span', 'ai-chat-panel__ptt-label');
  pttLabel.textContent = 'Hold to talk';

  const textRow = appendElement(root, 'form', 'ai-chat-panel__text-row');
  const textInput = appendElement(textRow, 'input', 'ai-chat-panel__text-input');
  textInput.type = 'text';
  textInput.placeholder = 'Type a message…';
  textInput.maxLength = AI_CHAT_TEXT_MAX_CHARS;
  textInput.setAttribute('aria-label', 'Type a message to the AI');
  const textSend = appendElement(textRow, 'button', 'ai-chat-panel__text-send');
  textSend.type = 'submit';
  textSend.textContent = 'Send';

  function sendTypedText() {
    const text = textInput.value.trim();
    if (!text || textSend.disabled) return;
    current.onSendText(text);
    textInput.value = '';
    textSend.disabled = true;
  }

  textRow.addEventListener('submit', (event) => {
    event.preventDefault();
    sendTypedText();
  });
  textInput.addEventListener('input', () => {
    textSend.disabled = textInput.disabled || textInput.value.trim().length === 0;
  });
  // Same reason as the PTT button: the tile itself is draggable/clickable.
  textRow.addEventListener('click', (event) => event.stopPropagation());
  textRow.addEventListener('wheel', (event) => event.stopPropagation());
  textInput.addEventListener('pointerdown', (event) => event.stopPropagation());

  stopButton.addEventListener('click', (event) => {
    event.preventDefault();
    event.stopPropagation();
    endPress();
    current.onStop();
  });
  stopButton.addEventListener('pointerdown', (event) => event.stopPropagation());

  // --- push-to-talk ---------------------------------------------------------
  // A stuck-open floor keeps the host tapping the room's microphone after the
  // user believes they let go, so EVERY way a press can end without a clean
  // pointerup releases it. `aiChatSession.ts` adds the page-level backstops
  // (tab hidden, pagehide, window blur, teardown).
  let pressed = false;

  function beginPress(event: Event) {
    event.preventDefault?.();
    event.stopPropagation?.();
    if (pressed || pttButton.disabled) return;
    pressed = true;
    const pointerEvent = event as PointerEvent;
    const capture = (pttButton as Partial<HTMLElement>).setPointerCapture;
    if (typeof capture === 'function' && typeof pointerEvent.pointerId === 'number') {
      try {
        capture.call(pttButton, pointerEvent.pointerId);
      } catch {
        // Capture is an improvement, not a requirement -- the release paths
        // below stand on their own.
      }
    }
    pttButton.classList.add('is-holding');
    current.onPttStart();
  }

  function endPress(event?: Event) {
    event?.stopPropagation?.();
    if (!pressed) return;
    pressed = false;
    pttButton.classList.remove('is-holding');
    current.onPttEnd();
  }

  pttButton.addEventListener('pointerdown', beginPress);
  pttButton.addEventListener('pointerup', endPress);
  pttButton.addEventListener('pointerleave', endPress);
  pttButton.addEventListener('pointercancel', endPress);
  pttButton.addEventListener('lostpointercapture', endPress);
  pttButton.addEventListener('blur', endPress);
  // Keyboard parity: space/enter hold the floor for as long as the key is down.
  pttButton.addEventListener('keydown', (event) => {
    const key = (event as KeyboardEvent).key;
    if (key !== ' ' && key !== 'Enter') return;
    if ((event as KeyboardEvent).repeat) return;
    beginPress(event);
  });
  pttButton.addEventListener('keyup', (event) => {
    const key = (event as KeyboardEvent).key;
    if (key !== ' ' && key !== 'Enter') return;
    endPress(event);
  });
  // The tile itself is draggable/clickable; never let a PTT gesture also
  // pin the tile or start a draw stroke.
  pttButton.addEventListener('click', (event) => {
    event.preventDefault();
    event.stopPropagation();
  });
  pttButton.addEventListener('wheel', (event) => event.stopPropagation());

  function renderTranscript(session: AiChatSessionState | null) {
    clearElement(transcript);
    const turns = session?.turns ?? [];
    transcript.hidden = turns.length === 0;
    for (const turn of turns) {
      const bubble = appendElement(transcript, 'div', `ai-chat-panel__turn is-${turn.role}`);
      bubble.dataset.role = turn.role;
      bubble.dataset.turnId = String(turn.id);
      const who = appendElement(bubble, 'span', 'ai-chat-panel__turn-role');
      who.textContent = turn.role === 'assistant' ? 'AI' : 'You & room';
      const text = appendElement(bubble, 'span', 'ai-chat-panel__turn-text');
      text.textContent = turn.text;
      bubble.classList.toggle('is-open', !turn.final);
    }
  }

  function update(
    nextOptions: AiChatPanelOptions,
    session: AiChatSessionState | null,
    localPttHeld: boolean,
  ) {
    current = nextOptions;
    if (root.parentElement !== current.tile) current.tile.appendChild(root);

    const active = session?.active === true;
    root.classList.toggle('is-active', active);
    root.dataset.windowId = String(current.windowId);
    root.dataset.owner = current.ownerIdentity;

    badgeText.textContent = active ? 'AI chat live' : 'AI chat';
    badge.classList.toggle('is-live', active);

    const secondsLeft = session?.secondsLeft ?? null;
    countdown.hidden = !active || secondsLeft === null;
    countdown.textContent = secondsLeft === null ? '' : formatAiChatCountdown(secondsLeft);
    stopButton.hidden = !active;
    stopButton.disabled = !active;

    disclosure.hidden = !active;

    const speaker = session?.activeSpeaker ?? null;
    const speakerIsLocal = !!speaker && speaker === current.localIdentity;
    const error = session?.error ?? null;

    let status = '';
    let warning = false;
    if (error) {
      status = aiChatEndReasonMessage(error);
      warning = !isNormalAiChatEnd(error);
    } else if (!active) {
      status = 'AI chat is not running for this window.';
    } else if (speaker && !speakerIsLocal) {
      status = `Listening to ${nameFor(speaker)}`;
    } else if (speaker && speakerIsLocal) {
      status = 'Listening to you';
    } else if (session?.startedBy) {
      status = `Started by ${nameFor(session.startedBy)}`;
    } else {
      status = 'Hold the button to talk to the assistant.';
    }
    statusLine.textContent = status;
    statusLine.classList.toggle('is-warning', warning);

    // Another participant holds the single serial floor: two speakers
    // interleaved corrupt the turn rather than mixing, so the control is
    // disabled rather than merely ignored.
    const floorTaken = !!speaker && !speakerIsLocal;
    pttButton.disabled = !active || floorTaken;
    pttButton.setAttribute('aria-disabled', pttButton.disabled ? 'true' : 'false');
    pttButton.setAttribute('aria-pressed', localPttHeld ? 'true' : 'false');
    pttLabel.textContent = localPttHeld
      ? 'Release to send'
      : floorTaken
        ? `${nameFor(speaker)} is talking`
        : 'Hold to talk';
    pttButton.title = pttLabel.textContent;
    pttButton.setAttribute('aria-label', pttLabel.textContent);
    pttButton.classList.toggle('is-holding', localPttHeld);
    // The controller is the authority on whether the floor is actually held;
    // a local `pressed` that outlived it (expired session, forced release)
    // must not keep the button latched.
    if (!localPttHeld) pressed = false;

    // Text never claims the floor, but a turn already open (anyone's PTT)
    // must not overlap a clientContent send -- the Rust side refuses it
    // regardless, this just avoids a round trip to discover that.
    const textDisabled = !active || !!speaker;
    textInput.disabled = textDisabled;
    textSend.disabled = textDisabled || textInput.value.trim().length === 0;

    renderTranscript(session);
  }

  current.tile.appendChild(root);
  update(options, null, false);

  return {
    update,
    destroy() {
      // Never tear down while still holding the floor.
      endPress();
      root.remove();
    },
  };
}
