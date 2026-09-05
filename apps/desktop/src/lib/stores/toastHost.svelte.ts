// Shared rune store for "is the ToastHost currently showing a toast?".
//
// Previously ToastHost.svelte broadcast this via a `petal-toast-host-visible`
// DOM CustomEvent on `window`, and /meeting/[room] listened for it to know
// whether to grow the pill-mode host window so a root-level resilience toast
// isn't clipped. Both sides live in the SAME webview, so a DOM event round-trip
// is unnecessary indirection — this rune store replaces it with a direct
// reactive read. ToastHost writes `toastHostState.visible`; consumers read it.

export const toastHostState = $state<{ visible: boolean }>({ visible: false });

export function setToastHostVisible(visible: boolean) {
  toastHostState.visible = visible;
}
