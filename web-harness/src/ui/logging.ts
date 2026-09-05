import { sessionLogCollector, type SessionLogCollector, type SessionLogContext } from './sessionLogCollector';

export type LogKind = 'info' | 'ok' | 'warn' | 'error';

export function createLogger(
  sessionLog: HTMLDivElement,
  getContext: () => SessionLogContext,
  collector: SessionLogCollector = sessionLogCollector
) {
  return function logEvent(message: string, kind: LogKind = 'info') {
    const context = getContext();
    const ts = new Date().toISOString();
    collector.record({
      ts,
      identity: context.identity,
      room: context.room,
      kind,
      message,
    });

    const line = document.createElement('div');
    line.className = 'log-line';
    const time = document.createElement('span');
    time.className = 'log-time';
    time.textContent = new Date().toLocaleTimeString();
    const text = document.createElement('span');
    text.className = kind === 'info' ? '' : `log-kind-${kind}`;
    text.textContent = message;
    line.appendChild(time);
    line.appendChild(text);
    sessionLog.insertBefore(line, sessionLog.firstChild);
  };
}
