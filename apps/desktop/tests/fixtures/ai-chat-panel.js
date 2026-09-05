import { mount, tick } from 'svelte';
import { mockIPC } from '@tauri-apps/api/mocks';
import { emit } from '@tauri-apps/api/event';
import AiChatPanel from '../../src/lib/components/AiChatPanel.svelte';

const calls = [];
const outcomes = {
  approve: true,
  resume: true,
  ptt: true,
};
let controlStatus = { sessionId: 77, standing: 'ask' };

mockIPC(
  (command, payload = {}) => {
    calls.push({ command, payload });
    switch (command) {
      case 'ai_chat_control_status':
        return controlStatus;
      case 'ai_chat_control_approve':
        if (outcomes.approve && payload.sessionScope) {
          controlStatus = { sessionId: payload.sessionId, standing: 'session' };
        }
        return outcomes.approve;
      case 'ai_chat_control_reject':
        controlStatus = { sessionId: payload.sessionId, standing: 'refused' };
        return true;
      case 'ai_chat_control_resume':
        if (outcomes.resume) controlStatus = { sessionId: payload.sessionId, standing: 'ask' };
        return outcomes.resume;
      case 'ai_chat_ptt_start':
        return outcomes.ptt;
      default:
        return undefined;
    }
  },
  { shouldMockEvents: true },
);

mount(AiChatPanel, { target: document.querySelector('#app') });

async function settle() {
  await tick();
  await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
}

async function send(event, payload) {
  await emit(event, payload);
  await settle();
}

window.aiChatPanelFixture = {
  calls,
  outcomes,
  settle,
  setControlStatus(standing, sessionId = 77) {
    controlStatus = { sessionId, standing };
  },
  live() {
    return send('ai-chat-state', { windowId: 42, state: { phase: 'live' } });
  },
  floor(activeSpeaker) {
    return send('ai-chat-state', { windowId: 42, activeSpeaker });
  },
  controlRequest(detail, requestId = 'fc_1') {
    return send('ai-chat-control-request', {
      windowId: 42,
      tool: 'window_type',
      requestId,
      sessionId: 77,
      detail,
    });
  },
  controlResolved(requestId = 'fc_1') {
    return send('ai-chat-control-resolved', {
      windowId: 42,
      requestId,
      ok: true,
      code: 'ok',
    });
  },
};

settle().then(() => {
  document.body.dataset.aiChatPanelReady = 'true';
});
