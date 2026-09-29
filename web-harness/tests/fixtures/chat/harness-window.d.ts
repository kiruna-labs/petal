// Shape of the browser client's automation hook as the chat live smoke drivers
// use it (web-harness/src/main.ts `harnessHook` + chat/setupChat.svelte.ts).
// One declaration for both drivers: they compile in the same tests program.
type ChatSmokeMsg = {
  id: string;
  text: string;
  sender: { identity: string; name: string | null };
  self: boolean;
  relayed: boolean;
  t: number;
  via: { id: string; name: string } | null;
  local: boolean;
};
type ChatSmokePacket = { at: string; from: string; name: string; type: string; text?: string; count?: number };
interface Window {
  __petalHarness: {
    room: {
      state: string;
      localParticipant: { identity: string };
      remoteParticipants: Map<string, { identity: string; name?: string }>;
      on(event: string, cb: (...args: unknown[]) => void): unknown;
    } | null;
    chat: { open: boolean; setOpen(o: boolean): void; messages(): readonly ChatSmokeMsg[]; send(t: string): Promise<void> } | null;
  };
  __chatPackets: ChatSmokePacket[];
}
