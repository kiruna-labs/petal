<!--
  Dedicated webview route for the floating native AI chat panel (#738).
-->
<script lang="ts">
  import AiChatPanel from '$lib/components/AiChatPanel.svelte';
  import { aiChatEndReasonMessage } from '$lib/data/aiChat';
  import type { AiChatEndReason } from '$lib/ipc';

  let endMessage = $state<string | null>(null);

  function handleEnded(reason: AiChatEndReason) {
    endMessage = aiChatEndReasonMessage(reason);
  }
</script>

<main class="panel-host">
  <AiChatPanel {endMessage} onEnded={handleEnded} />
</main>

<style>
  :global(html),
  :global(body) {
    margin: 0;
    padding: 0;
    width: 100%;
    height: 100%;
    overflow: hidden;
    background: transparent;
    user-select: none;
    -webkit-user-select: none;
  }
  .panel-host {
    width: 100vw;
    height: 100vh;
    box-sizing: border-box;
    display: flex;
    flex-direction: column;
  }
</style>
