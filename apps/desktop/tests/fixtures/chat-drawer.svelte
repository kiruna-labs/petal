<script lang="ts">
  import ChatDrawer from '@petal/shared/ui/components/ChatDrawer.svelte';
  import type { ChatMessage } from '@petal/shared/logic/chat';

  const base = Date.UTC(2026, 8, 14, 12, 0, 0);
  // A long name, a long unbroken token, a multi-line message, a relayed one,
  // and two consecutive lines from one sender (grouped under one name).
  let messages = $state<ChatMessage[]>([
    { id: 'm-000000001', text: 'Morning! Starting the review in a minute.', t: base, sender: { identity: 'mira-1', name: 'Mira Aleksandra Konstantinopoulou-Whitfield' }, self: false, relayed: true },
    { id: 'm-000000002', text: 'https://example.com/a/very/long/path/that/keeps/going/without/any/spaces/at/all/so/it/must/break/somewhere/or/overflow', t: base + 30_000, sender: { identity: 'theo-1', name: 'Theo' }, self: false, relayed: false },
    { id: 'm-000000003', text: 'Line one\nLine two\n\nAfter a blank line', t: base + 60_000, sender: { identity: 'theo-1', name: 'Theo' }, self: false, relayed: false },
    { id: 'm-000000004', text: 'On it 👍', t: base + 90_000, sender: { identity: 'me-1', name: 'Alex' }, self: true, relayed: false }
  ]);
  const sent: string[] = [];
  (window as unknown as { __chat: { messages: ChatMessage[]; sent: string[]; add: (m: ChatMessage) => void } }).__chat = {
    get messages() {
      return messages;
    },
    sent,
    add: (m) => (messages = [...messages, m])
  };
  let closed = $state(0);
  (window as unknown as { __closed: () => number }).__closed = () => closed;
</script>

<div style="height: 560px; width: 100%; display: flex;">
  <div style="flex: 1; min-width: 0; background: #0b0c0e;"></div>
  <aside style="flex: none; width: min(320px, 60%); height: 100%;" data-testid="chat-aside">
    <ChatDrawer
      {messages}
      onSend={(text) => {
        sent.push(text);
        messages = [...messages, { id: `m-sent-${sent.length}`, text, t: base + 120_000 + sent.length, sender: { identity: 'me-1', name: 'Alex' }, self: true, relayed: false }];
      }}
      onClose={() => closed++}
      now={() => base + 200_000}
      locale="en-US"
    />
  </aside>
</div>
