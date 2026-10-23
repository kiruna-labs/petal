<!--
  Dedicated webview route for the floating native AI chat panel (#738).
-->
<script lang="ts">
  import { shadowGutterFromSearch, shadowGutterPadding } from '$lib/shadowGutter';
  import AiChatPanel from '$lib/components/AiChatPanel.svelte';
  import { aiChatEndReasonMessage } from '$lib/data/aiChat';
  import type { AiChatEndReason } from '$lib/ipc';

  let endMessage = $state<string | null>(null);

  function handleEnded(reason: AiChatEndReason) {
    endMessage = aiChatEndReasonMessage(reason);
  }
  // Native shadow is off; pad the card so its CSS shadow fits the window's
  // transparent, click-through gutter (#406). No gutter on Windows.
  const gutterPadding = shadowGutterPadding(shadowGutterFromSearch());
</script>

<div class="shadow-gutter" style={gutterPadding}>
<main class="panel-host">
  <AiChatPanel {endMessage} onEnded={handleEnded} />
</main>
</div>

<style>
  .shadow-gutter {
    box-sizing: border-box;
    width: 100vw;
    height: 100vh;
  }

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
    width: 100%;
    height: 100%;
    box-sizing: border-box;
    display: flex;
    flex-direction: column;
  }
</style>
