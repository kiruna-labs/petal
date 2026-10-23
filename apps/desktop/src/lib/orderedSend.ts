// Serializes fire-and-forget sends so they reach the transport in call order.
//
// The control overlay used to call `invoke(remote_control_send)` once per input
// without waiting. Each invoke becomes its own async Tauri command, and those
// run concurrently, so a key Down and its Up sent in the same millisecond could
// reach the (ordered) LiveKit data channel Up-first. The host then replayed the
// Up before the Down, held the Down until the 1.2s stuck-input sweeper released
// it, and the synthetic release repeated the click -- collapsing a selection or
// leaving a key down. Each send now starts only after the previous one settled.
export type OrderedSender<T> = (value: T) => Promise<void>;

export function createOrderedSender<T>(
  deliver: (value: T) => Promise<unknown>
): OrderedSender<T> {
  let tail: Promise<void> = Promise.resolve();
  return (value: T) => {
    // A failed send must not stall the queue or reject the next caller.
    tail = tail.then(
      () => deliver(value).then(
        () => undefined,
        () => undefined
      )
    );
    return tail;
  };
}
