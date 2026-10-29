// #310: while this launch joins through the saved server, the home screen
// and the meeting keep a persistent "Custom server: <host>" line. Runtime
// env and the build bake are not announced — only `settings`, the quiet
// redirect an invite link would otherwise follow. Not a toast.
import { customServerIndicator, type BackendUrlSource } from '@petal/shared/logic/serverEndpoints';

export const customServerState = $state<{ label: string | null }>({ label: null });

export function noteCustomServer(view: {
  effectiveBackendUrl: string | null;
  backendSource: BackendUrlSource;
}): void {
  customServerState.label = customServerIndicator(view.effectiveBackendUrl, view.backendSource);
}
