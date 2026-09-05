// One local, auto-dismissing toast (issue #2/#8/#7). Replaces the three
// copy-pasted `{visible, message, timer}` triples this route used for the
// invite-copied confirmation, the camera-problem surface, and the
// join-failure/return notice. Each toast owns its own dismiss timer; call
// `dispose()` from the route's onDestroy to clear it.
//
// ToastHost (Rust resilience events) stays reserved for cross-process events;
// these are purely local, in-webview confirmations.

export interface LocalToast {
  /** Reactive: whether the toast is currently shown. */
  readonly visible: boolean;
  /** Reactive: the message to render. */
  readonly message: string;
  /** Show the toast with `message`, auto-dismissing after the factory's ms.
   * Pass `dismissMs: 0` to keep it visible until `hide()` (used by terminal
   * errors that carry a retry affordance -- an error that vanishes in 4s
   * before the user can act on it is not a working affordance). */
  show(message: string, dismissMs?: number): void;
  /** Hide the toast immediately and clear its dismiss timer. */
  hide(): void;
  /** Tear down the dismiss timer (call from onDestroy). */
  dispose(): void;
}

export function createLocalToast(dismissMs: number): LocalToast {
  let visible = $state(false);
  let message = $state('');
  let timer: ReturnType<typeof setTimeout> | undefined;

  return {
    get visible() {
      return visible;
    },
    get message() {
      return message;
    },
    show(next: string, showDismissMs?: number) {
      message = next;
      visible = true;
      if (timer) clearTimeout(timer);
      timer = undefined;
      const ms = showDismissMs ?? dismissMs;
      if (ms > 0) {
        timer = setTimeout(() => (visible = false), ms);
      }
    },
    hide() {
      visible = false;
      if (timer) clearTimeout(timer);
      timer = undefined;
    },
    dispose() {
      if (timer) clearTimeout(timer);
      timer = undefined;
    }
  };
}
