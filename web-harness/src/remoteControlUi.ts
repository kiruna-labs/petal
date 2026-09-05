import type { ActiveRemoteControl, HarnessContext } from './context.ts';
import type { RemoteControlCapability, RemoteControlMessage } from './trackNames.ts';
import {
  chunkRemoteText,
  EMPTY_REMOTE_CONTROL_MODIFIERS,
  isPasteChord,
  normalizedPointInContainedMedia,
  canonicalRemoteControlFingerprint,
  newRemoteControlInputId,
  remoteControlModifiers,
  remoteControlPublishOptions,
  encodeRemoteControlHotPath,
  decodeRemoteControlHotPath,
  fixedPointCoordinateKey,
  parseRemoteControlJson,
  remoteControlGrantEnvelopeIsValid,
  remoteControlStatusRestoresSession,
  remoteControlNegotiatedGrantMatchesRequest,
} from './remoteControl.ts';
import {
  applyLocalEchoKey,
  clampLocalEchoAnchor,
  LOCAL_ECHO_RIPPLE_FADE_MS,
  LOCAL_ECHO_TEXT_TIMEOUT_MS,
  nextLocalEchoRippleId,
  type EchoPoint,
} from '@petal/shared/logic/localEcho';

const remoteControlEncoder = new TextEncoder();

type RemoteControlPoint = { x: number; y: number };

interface PendingPointerMove {
  active: ActiveRemoteControl;
  point: RemoteControlPoint;
  button: number;
  buttons: number;
  modifiers: ReturnType<typeof remoteControlModifiers>;
}

interface PendingWheel {
  active: ActiveRemoteControl;
  point: RemoteControlPoint;
  deltaX: number;
  deltaY: number;
  deltaMode: 0 | 1 | 2;
  modifiers: ReturnType<typeof remoteControlModifiers>;
}

// ---------------------------------------------------------------------------
// Interactive remote control: request/release lifecycle, the per-tile pointer/
// wheel/key handlers that publish `petal.remote-control` messages, and the
// "Request control" affordance button. Shares its publish/seq/base primitives
// with the automation API (harnessApi.ts) via the returned helpers.
// ---------------------------------------------------------------------------
export function setupRemoteControlUi(ctx: HarnessContext) {
  const { state } = ctx;
  const { logEvent, showToast } = ctx.ui;
  const pendingDiscrete = new Map<string, {
    targetUserId: string; windowId: number; controlSessionId: string; inputSeq: number; operationFingerprint: string;
  }>();
  const resultFeedbackTimers = new Map<string, ReturnType<typeof setTimeout>>();
  const pendingDiscreteTimers = new Map<string, ReturnType<typeof setTimeout>>();

  function nextRemoteControlSeq(): number {
    state.remoteControlSeq =
      state.remoteControlSeq >= Number.MAX_SAFE_INTEGER ? 1 : state.remoteControlSeq + 1;
    return state.remoteControlSeq;
  }

  // -------------------------------------------------------------------------
  // Refs #378: local echo -- opt-in (state.localEchoEnabled, default OFF),
  // purely local DOM rendering layered onto the controlled tile. Zero wire
  // changes: every value here is read from events already being sent, and
  // nothing drawn here could be mistaken for real shared-app content (see
  // localEcho.ts's truth-over-appearance rationale). Mirrors
  // apps/desktop/src/routes/compositor/control/+page.svelte's local echo.
  // -------------------------------------------------------------------------
  let echoRippleSeq = 0;
  let echoPendingText = '';
  let echoAnchor: EchoPoint | null = null;
  let echoLastClickPoint: EchoPoint | null = null;
  let echoTextTimer: ReturnType<typeof setTimeout> | undefined;
  let echoTextEl: HTMLDivElement | null = null;

  function ensureLocalEchoLayer(tile: HTMLDivElement): HTMLDivElement {
    let layer = tile.querySelector<HTMLDivElement>(':scope > .local-echo-layer');
    if (!layer) {
      layer = document.createElement('div');
      layer.className = 'local-echo-layer';
      layer.setAttribute('aria-hidden', 'true');
      tile.appendChild(layer);
    }
    return layer;
  }

  function spawnEchoRipple(tile: HTMLDivElement, clientX: number, clientY: number): EchoPoint {
    const rect = tile.getBoundingClientRect();
    const point = { x: clientX - rect.left, y: clientY - rect.top };
    echoRippleSeq = nextLocalEchoRippleId(echoRippleSeq);
    const layer = ensureLocalEchoLayer(tile);
    const span = document.createElement('span');
    span.className = 'local-echo-ripple';
    span.style.left = `${point.x}px`;
    span.style.top = `${point.y}px`;
    layer.appendChild(span);
    setTimeout(() => span.remove(), LOCAL_ECHO_RIPPLE_FADE_MS);
    return point;
  }

  function spawnEchoKeyFlash(tile: HTMLDivElement) {
    const layer = ensureLocalEchoLayer(tile);
    const span = document.createElement('span');
    span.className = 'local-echo-key-flash';
    layer.appendChild(span);
    setTimeout(() => span.remove(), LOCAL_ECHO_RIPPLE_FADE_MS);
  }

  function clearEchoText() {
    echoPendingText = '';
    echoAnchor = null;
    if (echoTextTimer) {
      clearTimeout(echoTextTimer);
      echoTextTimer = undefined;
    }
    echoTextEl?.remove();
    echoTextEl = null;
  }

  function scheduleEchoTextClear() {
    if (echoTextTimer) clearTimeout(echoTextTimer);
    echoTextTimer = setTimeout(() => {
      echoTextTimer = undefined;
      clearEchoText();
    }, LOCAL_ECHO_TEXT_TIMEOUT_MS);
  }

  function renderEchoText(tile: HTMLDivElement) {
    const layer = ensureLocalEchoLayer(tile);
    if (!echoTextEl) {
      echoTextEl = document.createElement('div');
      echoTextEl.className = 'local-echo-text';
      const chars = document.createElement('span');
      chars.className = 'local-echo-text-chars';
      const badge = document.createElement('span');
      badge.className = 'local-echo-text-badge';
      badge.textContent = 'sent, unconfirmed';
      echoTextEl.append(chars, badge);
      layer.appendChild(echoTextEl);
    }
    const chars = echoTextEl.querySelector<HTMLSpanElement>('.local-echo-text-chars')!;
    chars.textContent = echoPendingText;
    if (echoAnchor) {
      echoTextEl.style.left = `${echoAnchor.x}px`;
      echoTextEl.style.top = `${echoAnchor.y}px`;
    }
  }

  function handleEchoKeydown(tile: HTMLDivElement, event: KeyboardEvent) {
    spawnEchoKeyFlash(tile);
    const nextPending = applyLocalEchoKey(echoPendingText, event);
    if (nextPending === null) return;
    echoPendingText = nextPending;
    if (!echoPendingText) {
      clearEchoText();
      return;
    }
    if (!echoAnchor) {
      const rect = tile.getBoundingClientRect();
      echoAnchor = clampLocalEchoAnchor(
        echoLastClickPoint ?? { x: rect.width / 2, y: rect.height * 0.6 },
        { width: rect.width, height: rect.height }
      );
    }
    renderEchoText(tile);
    scheduleEchoTextClear();
  }

  function clearLocalEcho(tile: HTMLDivElement | null) {
    clearEchoText();
    tile?.querySelector(':scope > .local-echo-layer')?.remove();
    echoLastClickPoint = null;
  }

  function remoteControlTargetFromTile(
    tile: HTMLDivElement
  ): { targetUserId: string; windowId: number } | null {
    const targetUserId = tile.dataset.owner?.trim() ?? '';
    const windowId = Number(tile.dataset.windowId);
    if (!targetUserId || !Number.isSafeInteger(windowId) || windowId < 1 || windowId > 0xffff_ffff) return null;
    return { targetUserId, windowId };
  }

  function activeRemoteControlForTile(tile: HTMLDivElement): ActiveRemoteControl | null {
    if (!state.activeRemoteControl || state.activeRemoteControl.tileId !== tile.id || !state.room) return null;
    return state.activeRemoteControl;
  }

  function updateRemoteControlAffordances() {
    document.querySelectorAll<HTMLButtonElement>('.remote-control-button').forEach((button) => {
      const tile = button.closest<HTMLDivElement>('.share-tile');
      const active = !!tile && state.activeRemoteControl?.tileId === tile.id;
      button.textContent = active ? 'Controlling' : 'Request control';
      button.classList.toggle('is-active', active);
      button.setAttribute('aria-pressed', active ? 'true' : 'false');
      button.title = active ? 'Stop remote control' : 'Request remote control';
      tile?.classList.toggle('remote-control-active', active);
      if (
        tile &&
        !active &&
        tile.classList.contains('has-remote-window-header') &&
        !tile.classList.contains('is-spotlight') &&
        tile.closest('.tiles.layout-spotlight')
      ) {
        ctx.cb.fitTileLabels(tile);
      }
    });
  }

  function tileForRemoteControlStatus(message: Extract<RemoteControlMessage, { kind: 'status' }>): HTMLDivElement | null {
    for (const tile of document.querySelectorAll<HTMLDivElement>('.share-tile')) {
      // Host status/result packets target the controller; the tile owner is
      // the host (`controllerId`). Accept the old target mapping as a
      // compatibility fallback for legacy native peers.
      if (tile.dataset.owner !== message.controllerId && tile.dataset.owner !== message.targetUserId) continue;
      if (Number(tile.dataset.windowId) !== message.windowId) continue;
      return tile;
    }
    return null;
  }

  function resultFeedback(failureCode: string | undefined): { status: string; message: string } | null {
    switch (failureCode) {
      case 'notForeground':
        return { status: failureCode, message: 'Bring the shared target to the foreground, then try again.' };
      case 'occluded':
        return { status: failureCode, message: 'The shared target is covered at that point.' };
      case 'integrityBlocked':
        return { status: failureCode, message: 'Windows blocked control because the target has higher privileges.' };
      case 'secureField':
        return { status: failureCode, message: 'Remote input is blocked for secure fields.' };
      case 'unsupportedRoute':
        return { status: failureCode, message: 'That control is not supported for this shared app.' };
      case 'staleShareInstance':
        return { status: failureCode, message: 'The shared target changed. Start remote control again.' };
      case 'injectionTimeout':
        return { status: failureCode, message: 'Windows did not accept the remote input in time.' };
      case 'targetOffScreen':
      case 'targetUnavailable':
        return { status: 'targetUnavailable', message: 'The shared target is unavailable.' };
      default:
        return failureCode ? { status: 'requestFailed', message: 'Remote input was not accepted.' } : null;
    }
  }
  function clearRemoteControlStatus(tile: HTMLDivElement) {
    clearTimeout(resultFeedbackTimers.get(tile.id));
    resultFeedbackTimers.delete(tile.id);
    delete tile.dataset.remoteControlStatus;
    delete tile.dataset.remoteControlStatusMessage;
    delete tile.dataset.remoteControlStatusSeq;
  }

  function showTransientRemoteControlFeedback(
    tile: HTMLDivElement,
    status: string,
    message: string,
    sequence: string
  ) {
    clearRemoteControlStatus(tile);
    tile.dataset.remoteControlStatus = status;
    tile.dataset.remoteControlStatusMessage = message;
    tile.dataset.remoteControlStatusSeq = sequence;
    resultFeedbackTimers.set(
      tile.id,
      setTimeout(() => {
        if (tile.dataset.remoteControlStatusSeq === sequence) clearRemoteControlStatus(tile);
      }, 3000)
    );
  }

  function handleRemoteControlPayload(payload: Uint8Array, senderIdentity?: string) {
    let message: RemoteControlMessage;
    const binary = decodeRemoteControlHotPath(payload, state.room?.localParticipant.identity ?? '', senderIdentity ?? '');
    if (binary) {
      message = binary;
    } else {
      const parsed = parseRemoteControlJson(new TextDecoder().decode(payload));
      if (!parsed) return;
      message = parsed;
    }
    if (message.v !== 1) return;
    if (message.kind === 'result') {
      const pending = pendingDiscrete.get(message.inputId);
      const correlated = !!pending
        && pending.targetUserId === message.controllerId
        && pending.windowId === message.windowId
        && pending.controlSessionId === message.controlSessionId
        && pending.inputSeq === message.inputSeq
        && pending.operationFingerprint === message.operationFingerprint;
      const knownOutcome = ['applied', 'submitted', 'unauthorized', 'grantExpired', 'targetUnavailable', 'targetOffScreen', 'accessibilityDenied', 'resolveFailed', 'replayFailed', 'superseded', 'malformed', 'admissionOverloaded'].includes(message.outcome);
      const localRecipient = state.room?.localParticipant.identity === message.targetUserId;
      if (!correlated || !knownOutcome || !localRecipient || (senderIdentity && senderIdentity !== message.controllerId)) return;
      pendingDiscrete.delete(message.inputId);
      clearTimeout(pendingDiscreteTimers.get(message.inputId));
      pendingDiscreteTimers.delete(message.inputId);
      const tile = Array.from(document.querySelectorAll<HTMLDivElement>('.share-tile')).find(
        (candidate) => candidate.dataset.owner === message.controllerId && Number(candidate.dataset.windowId) === message.windowId
      );
      if (tile) {
        tile.dataset.remoteControlResult = message.outcome;
        tile.dataset.remoteControlResultInputId = message.inputId;
        if (message.deliveryRoute) tile.dataset.remoteControlDeliveryRoute = message.deliveryRoute;
        else delete tile.dataset.remoteControlDeliveryRoute;
        if (message.failureCode) tile.dataset.remoteControlFailureCode = message.failureCode;
        else delete tile.dataset.remoteControlFailureCode;
        const feedback = resultFeedback(message.failureCode);
        if (feedback) {
          showTransientRemoteControlFeedback(
            tile,
            feedback.status,
            feedback.message,
            message.inputId
          );
        }
      }
      logEvent(`remote control result ${message.outcome} window_id=${message.windowId} input=${message.inputId}`, message.outcome === 'applied' ? 'ok' : 'warn');
      return;
    }
    if (message.kind !== 'status') return;
    // #370 corrective pass (Moderate finding): `parseRemoteControlJson`
    // already validates `status` against the canonical wire status list --
    // this used to be a second, hand-copied, shorter allowlist that silently
    // dropped the real `requestFailed`/`textTruncated` statuses. Binary
    // frames can never decode to kind 'status' in the first place, so the
    // JSON-path validation above is the only gate this needs.
    const localIdentity = state.room?.localParticipant.identity;
    // #808: an `active` status must be able to RE-ESTABLISH the session, not
    // only update one that already exists. A `stopped` for a superseded
    // request clears `state.activeRemoteControl` (see the stopped branch
    // below), and every later `active` was then ignored, because adoption is
    // guarded by `active &&`. The controller sat permanently out of control
    // while the host believed it was granting -- measured live: the harness
    // reported `{granted:false, grantToken:null}` with `active=null` against a
    // host that had just logged `status emitted (local+controller)
    // status='active'`, two milliseconds after a `stopped`. It also defeated
    // #371's reconnect re-emit, whose whole purpose is to restore a
    // controller whose data channel was recreated.
    //
    // The sender check is preserved, not skipped: the restore requires a tile
    // whose owner IS the LiveKit-verified sender, so a peer cannot conjure a
    // session for a window it does not own. A grant token is required too --
    // a tokenless `active` restores nothing.
    {
      const restoreTile = tileForRemoteControlStatus(message);
      if (
        remoteControlStatusRestoresSession(message, {
          hasActiveSession: Boolean(state.activeRemoteControl),
          localIdentity,
          senderIdentity,
          tileOwner: restoreTile?.dataset.owner
        }) &&
        restoreTile &&
        senderIdentity
      ) {
        const shareInstanceId = restoreTile.dataset.shareInstanceId;
        state.activeRemoteControl = {
          tileId: restoreTile.id,
          targetUserId: senderIdentity,
          windowId: message.windowId,
          pointerId: null,
          grantToken: null,
          ...(shareInstanceId
            ? {
                targetKind: restoreTile.dataset.sourceKind === 'display' ? 'display' : 'window',
                shareInstanceId
              }
            : {})
        };
        logEvent(
          `remote control session restored from host status window_id=${message.windowId}`,
          'ok'
        );
      }
    }
    const active = state.activeRemoteControl;
    // Security: `targetUserId`/`windowId` in the wire message are attacker-
    // controlled and are NOT authentication -- only the LiveKit-authenticated
    // sender identity (from the DataReceived participant, threaded in by the
    // caller) may mutate the local grant token, v2 control session, or tear
    // down the session. Without this check any room peer could spoof a
    // status packet naming our own identity as `targetUserId` to poison our
    // echoed grant token or v2 controlSessionId (silently disabling our
    // control, or making our next discrete op admit under an attacker-chosen
    // session), or force our session to stop -- exactly the class of attack
    // this per-grant token was added to close. Refs #288: the v2 fields
    // (`controlSessionId`/`resultCapability`) are folded into this SAME
    // sender-checked branch rather than the generic tile-rendering path
    const negotiatedGrantMatchesRequest = remoteControlNegotiatedGrantMatchesRequest(message, {
      targetKind: active?.targetKind,
      shareInstanceId: active?.shareInstanceId
    });
    // below, which has no sender check and must stay display-only.
    const senderIsHost = Boolean(active) && senderIdentity === active?.targetUserId;
    const grantEnvelopeIsValid = remoteControlGrantEnvelopeIsValid(message);
    // #802: this rejection used to be entirely silent -- an 'active' status
    // whose grant we refuse looks identical to no status at all, on both
    // sides. That cost a full live cycle to diagnose. Say so.
    if (active && senderIsHost && message.status === 'active' && !negotiatedGrantMatchesRequest) {
      logEvent(
        `remote control grant REJECTED window_id=${message.windowId} host_capabilities=${
          Array.isArray(message.hostCapabilities) ? message.hostCapabilities.length : 'absent'
        } result_capability_version=${message.resultCapability?.version ?? 'absent'}`,
        'warn'
      );
    }
    if (
      active &&
      senderIsHost &&
      message.windowId === active.windowId &&
      negotiatedGrantMatchesRequest &&
      message.targetUserId === localIdentity &&
      grantEnvelopeIsValid &&
      message.status === 'active'
    ) {
      if (typeof message.grantToken === 'string') {
        active.grantToken = message.grantToken;
      }
      if (message.controlSessionId) {
        active.controlSessionId = message.controlSessionId;
        active.resultCapability = message.resultCapability;
        active.nextInputSeq = 1;
        active.targetKind = message.targetKind;
        active.shareInstanceId = message.shareInstanceId;
        active.hostCapabilities = message.hostCapabilities ?? [];
      } else {
        delete active.controlSessionId;
        delete active.resultCapability;
        delete active.targetKind;
        delete active.shareInstanceId;
        delete active.hostCapabilities;
      }
      // #370 corrective pass (Bug C): remember whether THIS host advertised
      // hot-path support on this "active" status -- `publishRemoteControl`
      // will only switch to the binary encoding once this is true. A
      // not-yet-upgraded host's status packet never sets the field, so this
      // is `false` against it and we keep sending JSON.
      active.supportsBinaryHotPath = message.supportsBinaryHotPath === true;
    } else if (
      active &&
      senderIsHost &&
      message.windowId === active.windowId &&
      message.targetUserId === localIdentity &&
      ((grantEnvelopeIsValid && message.status === 'stopped') ||
        // Consent deny / structural refusal before any grant: the optimistic
        // `activeRemoteControl` placeholder (grantToken null) must not keep
        // reporting "active" (adversarial review P4). These carry no grant
        // envelope, so they are not gated on `grantEnvelopeIsValid`.
        (!active.grantToken &&
          (message.status === 'denied' ||
            message.status === 'requestUnavailable' ||
            message.status === 'disabled')))
    ) {
      delete active.controlSessionId;
      delete active.resultCapability;
      clearPendingRemoteControlInput();
      if (state.localEchoEnabled) {
        clearLocalEcho(document.getElementById(active.tileId) as HTMLDivElement | null);
      }
      state.activeRemoteControl = null;
      updateRemoteControlAffordances();
    }
    const tile = tileForRemoteControlStatus(message);
    if (!tile) return;
    if (message.status === 'active' || message.status === 'stopped') {
      clearRemoteControlStatus(tile);
      return;
    }
    tile.dataset.remoteControlStatus = message.status;
    tile.dataset.remoteControlStatusMessage = message.message;
    tile.dataset.remoteControlStatusSeq = String(message.seq);
  }

  function publishRemoteControl(message: RemoteControlMessage): Promise<void> {
    if (!state.room) return Promise.resolve();
    // #370 corrective pass (Bug C): never attempt the binary encoding unless
    // the CURRENT active session for this exact (windowId, targetUserId) has
    // observed the host advertise `supportsBinaryHotPath` on a real "active"
    // status packet. A not-yet-upgraded host never sets that field, so this
    // is false against it and every send falls through to JSON, exactly as
    // before this pass -- no negotiation round-trip, no risk of sending a
    // binary frame an old host's `serde_json::from_slice` can't parse.
    const active = state.activeRemoteControl;
    const hotPathCapable = Boolean(
      active &&
      active.targetUserId === message.targetUserId &&
      active.windowId === message.windowId &&
      active.supportsBinaryHotPath
    );
    const bytes =
      (hotPathCapable ? encodeRemoteControlHotPath(message) : null) ??
      remoteControlEncoder.encode(JSON.stringify(message));
    const options = remoteControlPublishOptions(message);
    return state.room.localParticipant.publishData(bytes, options).catch((err) => {
      if (
        (message.kind === 'pointer' && message.action === 'move' && (message.buttons ?? 0) === 0) ||
        message.kind === 'wheel'
      ) {
        return;
      }
      logEvent(`remote control publish failed: ${(err as Error).message ?? err}`, 'warn');
      throw err;
    });
  }

  function publishRemoteControlFromUi(message: RemoteControlMessage) {
    void publishRemoteControl(message).catch(() => {
      showToast('Remote input could not be sent');
    });
  }

  async function publishDiscreteRemoteControl(
    active: ActiveRemoteControl,
    message: Extract<RemoteControlMessage, { kind: 'pointer' | 'wheel' | 'key' | 'text' }>
  ) {
    // Mixed-version peers retain v1 semantics. Retry remains explicitly off;
    // a caller publishes once and terminal results are observational only.
    if (!active.controlSessionId || !active.resultCapability || active.resultCapability.retryEnabled) {
      await publishRemoteControl(message);
      return;
    }
    const inputSeq = active.nextInputSeq ?? 1;
    active.nextInputSeq = inputSeq + 1;
    const grant = { controlSessionId: active.controlSessionId, inputId: newRemoteControlInputId(), inputSeq };
    const operationFingerprint = await canonicalRemoteControlFingerprint(message, grant);
    pendingDiscrete.set(grant.inputId, {
      targetUserId: active.targetUserId,
      windowId: active.windowId,
      controlSessionId: grant.controlSessionId,
      inputSeq: grant.inputSeq,
      operationFingerprint
    });
    pendingDiscreteTimers.set(
      grant.inputId,
      setTimeout(() => {
        pendingDiscrete.delete(grant.inputId);
        pendingDiscreteTimers.delete(grant.inputId);
      }, 15_000)
    );
    try {
      await publishRemoteControl({
        ...message,
        controlSessionId: grant.controlSessionId,
        inputId: grant.inputId,
        inputSeq: grant.inputSeq,
        operationFingerprintVersion: 1,
        operationFingerprint
      });
    } catch (error) {
      pendingDiscrete.delete(grant.inputId);
      clearTimeout(pendingDiscreteTimers.get(grant.inputId));
      pendingDiscreteTimers.delete(grant.inputId);
      throw error;
    }
  }

  function publishDiscreteRemoteControlFromUi(
    active: ActiveRemoteControl,
    message: Extract<RemoteControlMessage, { kind: 'pointer' | 'wheel' | 'key' | 'text' }>
  ) {
    void publishDiscreteRemoteControl(active, message).catch(() => showToast('Remote input could not be sent'));
  }

  function remoteControlBase(active: ActiveRemoteControl) {
    if (!state.room) return null;
    // grantToken starts as null (see startRemoteControl) before any grant is
    // issued; normalize null to undefined so a not-yet-granted request omits
    // the key entirely, matching the wire's skip_serializing_if convention
    // and harnessApi.ts's remoteControlBaseForTarget.
    const grantToken = active.grantToken ?? undefined;
    return {
      v: 1 as const,
      targetUserId: active.targetUserId,
      controllerId: state.room.localParticipant.identity,
      windowId: active.windowId,
      seq: nextRemoteControlSeq(),
      ...(grantToken !== undefined ? { grantToken } : {}),
      ...(active.controlSessionId && active.targetKind && active.shareInstanceId
        ? {
            targetKind: active.targetKind,
            shareInstanceId: active.shareInstanceId,
            hostCapabilities: active.hostCapabilities ?? []
          }
        : {})
    };
  }

  function capableHostSupports(
    active: ActiveRemoteControl,
    capability: RemoteControlCapability
  ): boolean {
    return !active.controlSessionId || active.hostCapabilities?.includes(capability) === true;
  }

  let pointerMoveFrame = 0;
  let pendingPointerMove: PendingPointerMove | null = null;
  let wheelFrame = 0;
  let pendingWheel: PendingWheel | null = null;
  const lastSentMoveCoordinates = new Map<string, string>();
  // #373: tile ids currently mid IME composition (CJK, dead keys, emoji
  // picker). Belt-and-suspenders alongside `KeyboardEvent.isComposing` --
  // some engines don't reliably set `isComposing` on every keydown/keyup of
  // a composing sequence, so this tracks compositionstart/compositionend
  // explicitly too.
  const composingTiles = new Set<string>();

  function schedulePointerMoveFlush() {
    if (pointerMoveFrame) return;
    pointerMoveFrame = requestAnimationFrame(() => {
      pointerMoveFrame = 0;
      flushPendingPointerMove();
    });
  }

  function scheduleWheelFlush() {
    if (wheelFrame) return;
    wheelFrame = requestAnimationFrame(() => {
      wheelFrame = 0;
      flushPendingWheel();
    });
  }

  function clearPendingRemoteControlInput() {
    pendingPointerMove = null;
    pendingWheel = null;
    if (pointerMoveFrame) {
      cancelAnimationFrame(pointerMoveFrame);
      pointerMoveFrame = 0;
    }
    if (wheelFrame) {
      cancelAnimationFrame(wheelFrame);
      wheelFrame = 0;
    }
  }

  function flushPendingPointerMove() {
    const pending = pendingPointerMove;
    pendingPointerMove = null;
    if (!pending || state.activeRemoteControl !== pending.active) return;
    const coordinateKey = fixedPointCoordinateKey(pending.point.x, pending.point.y);
    const streamKey = `${pending.active.targetUserId}:${pending.active.windowId}`;
    if (lastSentMoveCoordinates.get(streamKey) === coordinateKey) return;
    lastSentMoveCoordinates.set(streamKey, coordinateKey);
    sendRemotePointerDraft(pending.active, 'move', pending.point, {
      button: pending.button,
      buttons: pending.buttons,
      modifiers: pending.modifiers,
    });
  }

  function flushPendingWheel() {
    const pending = pendingWheel;
    pendingWheel = null;
    if (!pending || state.activeRemoteControl !== pending.active) return;
    if (
      !capableHostSupports(pending.active, 'discreteScrollV1') &&
      !capableHostSupports(pending.active, 'uiaScroll')
    ) {
      return;
    }
    const base = remoteControlBase(pending.active);
    if (!base) return;
    const message = {
      ...base,
      kind: 'wheel' as const,
      x: pending.point.x,
      y: pending.point.y,
      deltaX: pending.deltaX,
      deltaY: pending.deltaY,
      deltaMode: pending.deltaMode,
      modifiers: pending.modifiers,
    };
    if (pending.active.controlSessionId) {
      publishDiscreteRemoteControlFromUi(pending.active, message);
    } else {
      publishRemoteControlFromUi(message);
    }
  }

  function stopRemoteControl(reason?: string) {
    const stopped = state.activeRemoteControl;
    if (!stopped) return;
    flushPendingPointerMove();
    flushPendingWheel();
    const base = remoteControlBase(stopped);
    if (base) publishRemoteControlFromUi({ ...base, kind: 'release' });
    clearPendingRemoteControlInput();
    for (const [inputId, pending] of pendingDiscrete) {
      if (
        pending.targetUserId === stopped.targetUserId &&
        pending.windowId === stopped.windowId
      ) {
        pendingDiscrete.delete(inputId);
        clearTimeout(pendingDiscreteTimers.get(inputId));
        pendingDiscreteTimers.delete(inputId);
      }
    }
    if (state.localEchoEnabled) {
      clearLocalEcho(document.getElementById(stopped.tileId) as HTMLDivElement | null);
    }
    composingTiles.delete(stopped.tileId);
    state.activeRemoteControl = null;
    updateRemoteControlAffordances();
    const stoppedTile =
      typeof document !== 'undefined' && typeof document.getElementById === 'function'
        ? (document.getElementById(stopped.tileId) as HTMLDivElement | null)
        : null;
    if (stoppedTile) setFocusHintVisible(stoppedTile, false);
    if (reason) {
      logEvent(`remote control stopped for ${stopped.targetUserId} window_id=${stopped.windowId}: ${reason}`);
    }
  }

  function startRemoteControl(tile: HTMLDivElement) {
    const target = remoteControlTargetFromTile(tile);
    if (!target || !state.room) return;
    if (state.activeRemoteControl?.tileId !== tile.id) stopRemoteControl();
    clearRemoteControlStatus(tile);
    const targetKind = tile.dataset.sourceKind === 'display' ? 'display' : 'window';
    const shareInstanceId = tile.dataset.shareInstanceId;
    state.activeRemoteControl = {
      tileId: tile.id,
      targetUserId: target.targetUserId,
      windowId: target.windowId,
      pointerId: null,
      grantToken: null,
      ...(shareInstanceId ? { targetKind, shareInstanceId } : {})
    };
    const base = remoteControlBase(state.activeRemoteControl);
    if (base) {
      publishRemoteControlFromUi({
        ...base,
        kind: 'request',
        ...(shareInstanceId
          ? {
              targetKind,
              shareInstanceId,
              controllerCapabilities: [
                'legacyControl',
                'discretePointerV1',
                'discreteScrollV1',
                'windowLocalPointer',
                'globalKeyboard',
                'uiaInvoke',
                'uiaScroll',
                'unicodeText'
              ] as const
            }
          : {})
      });
    }
    tile.focus({ preventScroll: true });
    updateRemoteControlAffordances();
    showToast('Remote control requested');
    logEvent(`remote control requested for ${target.targetUserId} window_id=${target.windowId}`, 'ok');
  }

  // #376 item 3: keydown/keyup are bound on the tile itself (bindRemoteControlHandlers
  // below) and only fire while the tile holds DOM focus -- pointer input keeps
  // working regardless (handlers are bound to the tile element, not gated on
  // focus), but keyboard silently stops if focus moves elsewhere while control
  // is still active. This surfaces that instead of leaving it invisible; any
  // pointerdown on the tile already refocuses it (handleRemoteControlPointerDown),
  // so the hint itself needs no click handling of its own.
  function ensureFocusHint(tile: HTMLDivElement): HTMLDivElement {
    let hint = tile.querySelector<HTMLDivElement>('.remote-control-focus-hint');
    if (!hint) {
      hint = document.createElement('div');
      hint.className = 'remote-control-focus-hint';
      hint.setAttribute('role', 'status');
      hint.setAttribute('aria-live', 'polite');
      hint.textContent = 'Click to resume control';
      tile.appendChild(hint);
    }
    return hint;
  }

  function setFocusHintVisible(tile: HTMLDivElement, visible: boolean) {
    ensureFocusHint(tile).classList.toggle('is-visible', visible);
  }

  function toggleRemoteControl(tile: HTMLDivElement) {
    if (state.activeRemoteControl?.tileId === tile.id) {
      stopRemoteControl('manual');
    } else {
      startRemoteControl(tile);
    }
  }

  function remoteControlPointForTile(
    tile: HTMLDivElement,
    event: Pick<PointerEvent | WheelEvent, 'clientX' | 'clientY'>,
    clamp: boolean
  ) {
    const video = tile.querySelector<HTMLVideoElement>('video');
    const rect = (video ?? tile).getBoundingClientRect();
    return normalizedPointInContainedMedia(
      { left: rect.left, top: rect.top, width: rect.width, height: rect.height },
      { width: video?.videoWidth ?? 0, height: video?.videoHeight ?? 0 },
      { x: event.clientX, y: event.clientY },
      { clamp }
    );
  }

  function sendRemotePointer(
    active: ActiveRemoteControl,
    event: PointerEvent,
    action: 'move' | 'down' | 'up',
    point: { x: number; y: number }
  ) {
    sendRemotePointerDraft(active, action, point, {
      button: event.button,
      buttons: action === 'up' ? 0 : event.buttons,
      // #373: `detail` is the DOM's own multi-click counter (1 = single,
      // 2 = double, ...), authoritative for the down that starts a gesture.
      // Only meaningful for down/up; omit it for move so old/new receivers
      // treat a plain drag identically.
      clickCount: action === 'move' ? undefined : Math.max(1, event.detail || 1),
      modifiers: remoteControlModifiers(event),
    });
  }

  function sendRemotePointerDraft(
    active: ActiveRemoteControl,
    action: 'move' | 'down' | 'up',
    point: RemoteControlPoint,
    input: {
      button: number;
      buttons: number;
      clickCount?: number;
      modifiers: ReturnType<typeof remoteControlModifiers>;
    }
  ) {
    const base = remoteControlBase(active);
    if (!base) return;
    if (active.controlSessionId && action !== 'up') return;
    if (active.controlSessionId && !capableHostSupports(active, 'uiaInvoke')) return;
    const message = {
      ...base,
      kind: 'pointer' as const,
      action: active.controlSessionId ? ('click' as const) : action,
      x: point.x,
      y: point.y,
      button: input.button,
      buttons: active.controlSessionId ? 0 : input.buttons,
      ...(input.clickCount !== undefined ? { clickCount: input.clickCount } : {}),
      modifiers: input.modifiers,
    };
    if (active.controlSessionId) publishDiscreteRemoteControlFromUi(active, message);
    else publishRemoteControlFromUi(message);
  }

  function handleRemoteControlPointerDown(event: PointerEvent) {
    const tile = event.currentTarget as HTMLDivElement;
    const active = activeRemoteControlForTile(tile);
    if (!active) return;
    const point = remoteControlPointForTile(tile, event, false);
    if (!point) return;
    event.preventDefault();
    tile.focus({ preventScroll: true });
    active.pointerId = event.pointerId;
    try {
      tile.setPointerCapture(event.pointerId);
    } catch {
      // Pointer capture can fail for synthetic events; input still publishes.
    }
    if (state.localEchoEnabled) {
      echoLastClickPoint = spawnEchoRipple(tile, event.clientX, event.clientY);
    }
    sendRemotePointer(active, event, 'down', point);
  }

  function handleRemoteControlPointerMove(event: PointerEvent) {
    const tile = event.currentTarget as HTMLDivElement;
    const active = activeRemoteControlForTile(tile);
    if (!active) return;
    if (active.pointerId !== null && event.pointerId !== active.pointerId) return;
    const point = remoteControlPointForTile(tile, event, true);
    if (!point) return;
    event.preventDefault();
    pendingPointerMove = {
      active,
      point,
      button: event.button,
      buttons: event.buttons,
      modifiers: remoteControlModifiers(event),
    };
    schedulePointerMoveFlush();
  }

  function handleRemoteControlPointerUp(event: PointerEvent) {
    const tile = event.currentTarget as HTMLDivElement;
    const active = activeRemoteControlForTile(tile);
    if (!active) return;
    if (active.pointerId !== null && event.pointerId !== active.pointerId) return;
    const point = remoteControlPointForTile(tile, event, true);
    if (!point) return;
    event.preventDefault();
    flushPendingPointerMove();
    sendRemotePointer(active, event, 'up', point);
    try {
      tile.releasePointerCapture(event.pointerId);
    } catch {
      // Safe to ignore: the pointer may not have been captured.
    }
    active.pointerId = null;
  }

  function handleRemoteControlWheel(event: WheelEvent) {
    const tile = event.currentTarget as HTMLDivElement;
    const active = activeRemoteControlForTile(tile);
    if (!active) return;
    const point = remoteControlPointForTile(tile, event, false);
    if (!point) return;
    event.preventDefault();
    event.stopPropagation();
    const deltaMode = event.deltaMode === 1 || event.deltaMode === 2 ? event.deltaMode : 0;
    if (pendingWheel && (pendingWheel.active !== active || pendingWheel.deltaMode !== deltaMode)) {
      flushPendingWheel();
    }
    pendingWheel = {
      active,
      point,
      deltaX: (pendingWheel?.deltaX ?? 0) + event.deltaX,
      deltaY: (pendingWheel?.deltaY ?? 0) + event.deltaY,
      deltaMode,
      modifiers: remoteControlModifiers(event),
    };
    // Throttled to once per animation-frame batch (matches the rAF-coalesced
    // send cadence via wheelFrame below) so a fast scroll doesn't flood the
    // tile with ripples.
    if (state.localEchoEnabled && !wheelFrame) {
      spawnEchoRipple(tile, event.clientX, event.clientY);
    }
    scheduleWheelFlush();
  }

  function handleRemoteControlKey(event: KeyboardEvent, action: 'down' | 'up') {
    const tile = event.currentTarget as HTMLDivElement;
    const active = activeRemoteControlForTile(tile);
    if (!active) return;
    // #373: an IME composition (CJK, dead keys, emoji picker) fires
    // keydown/keyup for every intermediate keystroke -- relaying those as
    // discrete `key` messages would replay raw, uncommitted input on the
    // host. Suppress them; the composed result is sent once as a `text`
    // message on compositionend instead. Deliberately do NOT
    // preventDefault/stopPropagation here so the browser's own composition
    // UI (candidate window) keeps working normally.
    if (event.isComposing || composingTiles.has(tile.id)) return;
    if (isPasteChord(event)) {
      // #375: the controller pastes ITS OWN clipboard (see
      // pasteControllerClipboard below) instead of forwarding the raw Cmd+V
      // keystroke -- forwarding it too would double-paste (once from our
      // `text` message, once from the host's classify_text_shortcut/
      // TextShortcut::Paste AX path).
      event.preventDefault();
      event.stopPropagation();
      if (action === 'down' && !event.repeat) {
        flushPendingPointerMove();
        flushPendingWheel();
        void pasteControllerClipboard(active);
      }
      return;
    }
    if (!capableHostSupports(active, 'globalKeyboard')) return;
    const base = remoteControlBase(active);
    if (!base) return;
    event.preventDefault();
    event.stopPropagation();
    flushPendingPointerMove();
    flushPendingWheel();
    if (state.localEchoEnabled && action === 'down') {
      handleEchoKeydown(tile, event);
    }
    publishDiscreteRemoteControlFromUi(active, {
      ...base,
      kind: 'key',
      action,
      key: event.key,
      code: event.code,
      repeat: action === 'down' ? event.repeat : false,
      location: event.location,
      modifiers: remoteControlModifiers(event),
    });
  }

  async function pasteControllerClipboard(active: ActiveRemoteControl) {
    // #375: text-only v1, read on the explicit Cmd+V gesture above -- never a
    // background/auto-synced clipboard read.
    const readClipboardText = navigator.clipboard?.readText?.bind(navigator.clipboard);
    if (!readClipboardText) return;
    let text = '';
    try {
      text = (await readClipboardText()) ?? '';
    } catch {
      // Permission denied / no text on the clipboard -- silently no-op,
      // matching publishRemoteControlFromUi's other best-effort failure modes.
      return;
    }
    if (!text) return;
    if (state.activeRemoteControl !== active) return;
    if (!capableHostSupports(active, 'unicodeText')) return;
    const base = remoteControlBase(active);
    if (!base) return;
    publishDiscreteRemoteControlFromUi(active, {
      ...base,
      kind: 'text',
      text,
      modifiers: EMPTY_REMOTE_CONTROL_MODIFIERS,
    });
  }

  async function sendRemoteComposedText(active: ActiveRemoteControl, text: string) {
    if (!capableHostSupports(active, 'unicodeText')) return;
    for (const chunk of chunkRemoteText(text)) {
      const base = remoteControlBase(active);
      if (!base) return;
      try {
        await publishDiscreteRemoteControl(active, {
          ...base,
          kind: 'text',
          text: chunk,
          modifiers: EMPTY_REMOTE_CONTROL_MODIFIERS,
        });
      } catch {
        showToast('Remote input could not be sent');
        return;
      }
    }
  }

  function handleRemoteControlCompositionStart(event: CompositionEvent) {
    const tile = event.currentTarget as HTMLDivElement;
    if (!activeRemoteControlForTile(tile)) return;
    composingTiles.add(tile.id);
  }

  function handleRemoteControlCompositionEnd(event: CompositionEvent) {
    const tile = event.currentTarget as HTMLDivElement;
    composingTiles.delete(tile.id);
    const active = activeRemoteControlForTile(tile);
    if (!active) return;
    const text = event.data ?? '';
    if (!text) return;
    flushPendingPointerMove();
    flushPendingWheel();
    void sendRemoteComposedText(active, text);
  }

  function bindRemoteControlHandlers(tile: HTMLDivElement) {
    if (tile.dataset.remoteControlBound === '1') return;
    tile.dataset.remoteControlBound = '1';
    tile.addEventListener('pointerdown', handleRemoteControlPointerDown);
    tile.addEventListener('pointermove', handleRemoteControlPointerMove);
    tile.addEventListener('pointerup', handleRemoteControlPointerUp);
    tile.addEventListener('pointercancel', handleRemoteControlPointerUp);
    tile.addEventListener('wheel', handleRemoteControlWheel, { passive: false });
    // #450 desktop uses a window-level listener because its route has no text
    // inputs. Keep this harness tile-scoped: the browser UI contains real chat
    // and settings inputs, whose keystrokes must never enter remote control.
    tile.addEventListener('keydown', (event) => handleRemoteControlKey(event, 'down'));
    tile.addEventListener('keyup', (event) => handleRemoteControlKey(event, 'up'));
    tile.addEventListener('focusin', () => setFocusHintVisible(tile, false));
    tile.addEventListener('focusout', () => {
      if (activeRemoteControlForTile(tile)) setFocusHintVisible(tile, true);
    });
    tile.addEventListener('compositionstart', handleRemoteControlCompositionStart);
    tile.addEventListener('compositionend', handleRemoteControlCompositionEnd);
  }

  function ensureRemoteControlAffordance(tile: HTMLDivElement) {
    const target = remoteControlTargetFromTile(tile);
    if (!target) return;
    tile.tabIndex = 0;
    bindRemoteControlHandlers(tile);

    let button = tile.querySelector<HTMLButtonElement>('.remote-control-button');
    if (!button) {
      button = document.createElement('button');
      button.type = 'button';
      button.className = 'remote-control-button';
      button.addEventListener('pointerdown', (event) => event.stopPropagation());
      button.addEventListener('wheel', (event) => event.stopPropagation());
      button.addEventListener('keydown', (event) => event.stopPropagation());
      button.addEventListener('keyup', (event) => event.stopPropagation());
      button.addEventListener('click', (event) => {
        event.preventDefault();
        event.stopPropagation();
        toggleRemoteControl(tile);
      });
      tile.appendChild(button);
    }
    updateRemoteControlAffordances();
  }

  return {
    // cross-module callbacks
    stopRemoteControl,
    startRemoteControl,
    handleRemoteControlPayload,
    activeRemoteControlForTile,
    ensureRemoteControlAffordance,
    // shared primitives for harnessApi.ts
    nextRemoteControlSeq,
    remoteControlTargetFromTile,
    publishRemoteControl,
  };
}
