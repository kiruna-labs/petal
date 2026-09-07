// Recover the SFU-stamped sender identity of a data packet when livekit-client
// hands `RoomEvent.DataReceived` an undefined `participant`.
//
// kiruna-labs/petal#2: livekit-client resolves a packet's sender with
// `remoteParticipants.get(packet.participantIdentity)` and, when that lookup
// misses, emits `DataReceived` with `participant: undefined` -- the identity
// the SFU stamped on the packet is dropped. The lookup misses for the whole
// window between a peer's full reconnect and the next ParticipantUpdate that
// names it: `handleParticipantUpdates` deletes the map entry BY IDENTITY on any
// DISCONNECTED info, so the old session's late disconnect also removes the NEW
// session's entry. Measured live (run 33997561478): every host status for
// ~400ms after the host's reconnect arrived with no sender, the controller's
// sender-authentication gate (correctly) ignored the `active` that carried the
// grant token, and the optimistic placeholder sat tokenless -- labelled
// "Controlling" -- while the host dropped every input as tokenless (#580).
//
// `packet.participantIdentity` is written by the SFU on relay; it is the very
// field livekit-client trusts for its own lookup, so it is the same trust
// anchor as `participant.identity` -- not a payload field a peer can forge.
//
// Mechanics: the engine emits `dataPacketReceived` to its listeners in
// registration order, and Room's own listener emits `RoomEvent.DataReceived`
// synchronously from inside that call. A listener PREPENDED ahead of Room's
// therefore runs for the same packet immediately before `DataReceived` fires;
// `take()` inside the `DataReceived` handler reads the identity and clears it.
// Only `prependListener` gives that ordering -- an appended listener would run
// AFTER `DataReceived` and label the NEXT packet with THIS packet's identity --
// so without `prependListener` nothing is attached and the fallback is simply
// absent (never wrong).

export const ENGINE_DATA_PACKET_RECEIVED_EVENT = 'dataPacketReceived';

/** The slice of `@livekit/protocol`'s `DataPacket` this module reads. */
export interface DataPacketLike {
  participantIdentity?: string;
  value?: { case?: string };
}

interface DataPacketEngineLike {
  prependListener?: (event: string, listener: (packet: DataPacketLike) => void) => unknown;
}

export interface SfuSenderIdentityResolver {
  /**
   * Prepend the capture listener on `engine`. Idempotent per engine object --
   * call it again after `Room.connect()` resolves, because a Room recreates a
   * closed engine there. Returns false when `engine` cannot host the listener.
   */
  attach(engine: unknown): boolean;
  /**
   * Identity stamped on the user packet the engine is delivering right now,
   * then clears it. Call it on EVERY `DataReceived`, even when the SDK did
   * resolve a participant, so a value can never carry over to a later packet.
   */
  take(): string | undefined;
}

export function createSfuSenderIdentityResolver(): SfuSenderIdentityResolver {
  const attached = new WeakSet<object>();
  let pending: string | undefined;
  return {
    attach(engine) {
      if (!engine || typeof engine !== 'object') return false;
      const candidate = engine as DataPacketEngineLike;
      if (typeof candidate.prependListener !== 'function') return false;
      if (attached.has(engine)) return true;
      attached.add(engine);
      candidate.prependListener(ENGINE_DATA_PACKET_RECEIVED_EVENT, (packet) => {
        pending =
          packet?.value?.case === 'user' &&
          typeof packet.participantIdentity === 'string' &&
          packet.participantIdentity.length > 0
            ? packet.participantIdentity
            : undefined;
      });
      return true;
    },
    take() {
      const identity = pending;
      pending = undefined;
      return identity;
    }
  };
}
