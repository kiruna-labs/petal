import type { LogKind } from './logging';

export type SessionLogContext = {
  identity?: string;
  room?: string;
};

export type SessionLogEntry = SessionLogContext & {
  ts: string;
  kind: LogKind;
  message: string;
};

// #256: 10-minute soak runs need more history than the old 500-entry cap.
const DEFAULT_CAPACITY = 5000;
const UNKNOWN_FIELD = '-';

export class SessionLogCollector {
  readonly capacity: number;
  private entries: SessionLogEntry[] = [];

  constructor(capacity = DEFAULT_CAPACITY) {
    this.capacity = capacity;
  }

  record(entry: SessionLogEntry) {
    this.entries.push(entry);
    if (this.entries.length > this.capacity) {
      this.entries.splice(0, this.entries.length - this.capacity);
    }
  }

  getEntries() {
    return [...this.entries];
  }

  exportText() {
    return this.entries.map(formatSessionLogEntry).join('\n');
  }

  exportBlob() {
    return new Blob([this.exportText()], { type: 'text/plain;charset=utf-8' });
  }
}

export const sessionLogCollector = new SessionLogCollector();

export function formatSessionLogEntry(entry: SessionLogEntry) {
  const identity = fieldForLog(entry.identity);
  const room = fieldForLog(entry.room);
  const message = entry.message.replace(/\r?\n/g, '\\n');
  return `${entry.ts} ${identity} ${room} [${entry.kind}] ${message}`;
}

export function createSessionLogFilename(context: SessionLogContext, date = new Date()) {
  const identity = fieldForFilename(context.identity, 'unknown-identity');
  const room = fieldForFilename(context.room, 'unknown-room');
  const ts = date.toISOString().replace(/[:.]/g, '-');
  return `petal-session-${identity}-${room}-${ts}.log`;
}

function fieldForLog(value: string | undefined) {
  const trimmed = value?.trim();
  return trimmed ? trimmed : UNKNOWN_FIELD;
}

function fieldForFilename(value: string | undefined, fallback: string) {
  const trimmed = value?.trim();
  if (!trimmed) return fallback;
  return trimmed.replace(/[^a-zA-Z0-9._-]+/g, '-').replace(/^-+|-+$/g, '') || fallback;
}
