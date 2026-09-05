// Mounts the SHARED Svelte <Toast> (shared/ui/components/Toast.svelte — the
// same component the desktop app renders) into the web client's #toast
// wrapper, replacing the old textContent-based toast. This is the proof that
// web-harness consumes the shared component library: one Toast component,
// two clients.
import { mount, unmount } from 'svelte';
import Toast from '@petal/shared/ui/components/Toast.svelte';

let mounted: ReturnType<typeof mount> | null = null;
let dismissTimer: ReturnType<typeof setTimeout> | undefined;

/** Optional inline action (e.g. "Bring to foreground" on the #679 remote
 * share notice, mirroring the desktop pill's "Bring to foreground" link) --
 * forwarded straight to Toast's own `actionLabel`/`onAction` props. */
export interface SharedToastAction {
  actionLabel: string;
  onAction: () => void;
}

/**
 * Renders the shared Toast pill into `host`, clears any previous one, and
 * auto-dismisses after `dismissMs` (matching the previous textContent toast's
 * 2500ms default). The `host` element keeps the positioning/wrapping CSS
 * (`.toast` in style.css); the pill supplies the visuals.
 */
export function showSharedToast(
  host: HTMLElement,
  message: string,
  dismissMs = 2500,
  action?: SharedToastAction
): void {
  clearTimeout(dismissTimer);
  if (mounted !== null) unmount(mounted);
  mounted = mount(Toast, {
    target: host,
    props: {
      variant: 'info',
      message,
      actionLabel: action?.actionLabel,
      onAction: action?.onAction
    }
  });
  host.classList.remove('hidden');
  dismissTimer = setTimeout(() => {
    if (mounted !== null) unmount(mounted);
    mounted = null;
    host.classList.add('hidden');
  }, dismissMs);
}
