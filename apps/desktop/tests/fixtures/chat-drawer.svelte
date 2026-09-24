<script lang="ts">
  import ChatDrawer from '@petal/shared/ui/components/ChatDrawer.svelte';
  import type { ChatCommandOption, ChatMessage } from '@petal/shared/logic/chat';

  const base = Date.UTC(2026, 8, 14, 12, 0, 0);
  const timer = { id: 'petal.timer', name: 'Timer' };
  const person = (identity: string, name: string) => ({ identity, name });
  // A long name, a long unbroken token, a multi-line message, a relayed one,
  // two consecutive lines from one sender (grouped under one name), then a
  // plugin post by that same sender (NOT grouped: it carries `via`) and a
  // plugin's private answer to this user.
  let messages = $state<ChatMessage[]>([
    { id: 'm-000000001', text: 'Morning! Starting the review in a minute.', t: base, sender: person('mira-1', 'Mira Aleksandra Konstantinopoulou-Whitfield'), self: false, relayed: true, via: null, local: false },
    { id: 'm-000000002', text: 'https://example.com/a/very/long/path/that/keeps/going/without/any/spaces/at/all/so/it/must/break/somewhere/or/overflow', t: base + 30_000, sender: person('theo-1', 'Theo'), self: false, relayed: false, via: null, local: false },
    { id: 'm-000000003', text: 'Line one\nLine two\n\nAfter a blank line', t: base + 60_000, sender: person('theo-1', 'Theo'), self: false, relayed: false, via: null, local: false },
    { id: 'm-000000004', text: 'On it 👍', t: base + 90_000, sender: person('me-1', 'Alex'), self: true, relayed: false, via: null, local: false },
    { id: 'p-000000005', text: '⏱ Timer started: standup, 5 min', t: base + 100_000, sender: person('theo-1', 'Theo'), self: false, relayed: false, via: timer, local: false },
    { id: 'local-000006', text: 'Usage: /timer 5m [label], /timer list, /timer cancel [label].', t: base + 110_000, sender: person('me-1', 'Alex'), self: true, relayed: false, via: timer, local: true }
  ]);
  const commands: ChatCommandOption[] = [
    { name: 'timer', usage: '5m [label] | list | cancel', description: 'Start a countdown everyone can see', pluginId: 'petal.timer', pluginName: 'Timer', source: 'builtin' },
    { name: 'tally', usage: '', description: 'Count hands for a quick decision in the meeting', pluginId: 'acme.tally', pluginName: 'Tally for Teams Pro', source: 'registry' }
  ];
  const sent: string[] = [];
  const ran: string[] = [];
  (window as unknown as { __chat: { messages: ChatMessage[]; sent: string[]; ran: string[]; add: (m: ChatMessage) => void } }).__chat = {
    get messages() {
      return messages;
    },
    sent,
    ran,
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
      {commands}
      onSend={(text) => {
        sent.push(text);
        messages = [...messages, { id: `m-sent-${sent.length}`, text, t: base + 120_000 + sent.length, sender: person('me-1', 'Alex'), self: true, relayed: false, via: null, local: false }];
      }}
      onCommand={(name, args) => {
        ran.push(`${name}|${args}`);
        return name === 'tally' ? { ok: false, message: 'Tally for Teams Pro is still starting. Try again in a moment.' } : { ok: true };
      }}
      onClose={() => closed++}
      now={() => base + 200_000}
      locale="en-US"
    />
  </aside>
</div>
