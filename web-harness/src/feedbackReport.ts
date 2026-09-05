import type { SubmitData } from '@userdispatch/sdk';

import type { SensitiveStringRegistry } from './sensitiveStrings.ts';
import { sessionLogCollector } from './ui/sessionLogCollector.ts';

// #293 originally forbade this module from touching the session log at all:
// the upload was a fixed, code-only diagnostic document and raw logs stayed a
// local-only download. That policy was reversed deliberately (owner decision,
// 2026-08-13: "I want us to collect logs easily and automatically") because it
// left every user-reported incident undiagnosable -- see #788, where two P0s
// in three days produced no retrievable evidence from the web client at all.
//
// What replaces it is NOT "no log" but "no UNSCRUBBED log". Every byte of log
// text crosses this boundary through `SensitiveStringRegistry.scrubForReporting`
// (the same scrubber the local download uses), is capped to a recent tail, and
// is attached ONLY when the user ticks the diagnostics consent box. The
// invariant is enforced by test, not by convention -- see
// `tests/feedbackReport.test.ts`'s scrubbing guard, which replaced the old
// cannot-import guard rather than simply deleting it.
export const FEEDBACK_DIAGNOSTICS_FILENAME = 'petal-diagnostics.txt';
export const FEEDBACK_DIAGNOSTICS_TYPE = 'text/plain;charset=utf-8';
/** Most-recent slice of the session log carried with a report. Mirrors the
 * native client's 256 KiB tail (`logging.rs`'s
 * `FEEDBACK_ATTACHMENT_LOG_TAIL_BYTES`) at browser scale. */
export const FEEDBACK_MAX_LOG_TAIL_BYTES = 128 * 1024;
export const FEEDBACK_MAX_ATTACHMENT_BYTES = 192 * 1024;
export const FEEDBACK_MAX_MESSAGE_CHARS = 2_000;
/** Separates the closed-schema header from the scrubbed log tail. Exported so
 * the boundary tests split on the same string production writes. */
export const FEEDBACK_LOG_SECTION_HEADING = '=== session log (redacted) ===';
const DIAGNOSTICS_SCHEMA_VERSION = 2;

export type FeedbackEventCode = 'feedback_opened' | 'feedback_submitted';
const FEEDBACK_EVENT_CODES = new Set<FeedbackEventCode>(['feedback_opened', 'feedback_submitted']);

export type FeedbackRuntimeState = {
  connected: boolean;
  sharing: boolean;
  screenSharing: boolean;
};

export type FeedbackAdapter = {
  submit: (publicKey: string, data: SubmitData) => Promise<void>;
};

export function isValidUserDispatchPublicKey(value: string | null | undefined): value is string {
  return typeof value === 'string' && /^pk_[A-Za-z0-9_-]{8,}$/.test(value.trim());
}

function fixedConnectionState(state: FeedbackRuntimeState): 'connected' | 'disconnected' {
  return state.connected ? 'connected' : 'disconnected';
}

function normalizedBoundedText(value: string, maxLength: number): string {
  return value
    .normalize('NFKC')
    .replace(/[\u0000-\u001F\u007F]/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .slice(0, maxLength);
}

/**
 * Serializes a deliberately closed schema. Unknown inputs are not accepted by
 * this API, so neither room/identity data nor arbitrary event detail can
 * accidentally cross the Blob boundary.
 */
export function buildFeedbackDiagnostics(
  state: FeedbackRuntimeState,
  eventCodes: readonly string[],
  timestamp = new Date().toISOString()
): string {
  const codes = eventCodes.filter((code): code is FeedbackEventCode => FEEDBACK_EVENT_CODES.has(code as FeedbackEventCode)).slice(-20);
  return JSON.stringify({
    schema_version: DIAGNOSTICS_SCHEMA_VERSION,
    connection_state: fixedConnectionState(state),
    timestamp: normalizedBoundedText(timestamp, 40),
    event_codes: codes,
  });
}

/**
 * The most recent `FEEDBACK_MAX_LOG_TAIL_BYTES` of the session log, scrubbed.
 *
 * Scrubbing is applied to the WHOLE text before truncation, so a value that
 * straddles the cut cannot survive as a half-redacted fragment. The tail is
 * taken from the end because an incident is described by what happened last.
 * Truncation is marked so a reader never mistakes a clipped log for the start
 * of the session.
 *
 * The cut is measured in real UTF-8 BYTES, not `String.length`. A log full of
 * non-ASCII text -- a Japanese window title, an accented display name, an emoji
 * -- runs up to 4 bytes per code unit, so a length-based cap let the document
 * blow past `FEEDBACK_MAX_ATTACHMENT_BYTES` and throw. That failure is silent
 * to the user (`FeedbackReportController.submit` catches it and shows only
 * "Could not send feedback"), so those users could never file a diagnostics
 * report at all.
 */
export function buildScrubbedLogTail(
  logText: string,
  registry: SensitiveStringRegistry,
  maxBytes = FEEDBACK_MAX_LOG_TAIL_BYTES
): string {
  const scrubbed = registry.scrubForReporting(logText);
  const encoded = new TextEncoder().encode(scrubbed);
  if (encoded.length <= maxBytes) return scrubbed;
  // A byte cut can land mid-character; the decoder replaces the orphaned
  // sequence with U+FFFD rather than throwing. Redaction markers are ASCII, so
  // this can never split one into something that reads as unredacted.
  const tail = new TextDecoder('utf-8').decode(encoded.slice(encoded.length - maxBytes));
  return `[earlier log lines omitted — showing the last ${maxBytes} bytes]\n${tail}`;
}

export function buildFeedbackAttachment(
  state: FeedbackRuntimeState,
  eventCodes: readonly string[],
  registry: SensitiveStringRegistry,
  options: { timestamp?: string; logText?: string } = {}
): Blob {
  const diagnostics = buildFeedbackDiagnostics(state, eventCodes, options.timestamp);
  // Injectable so the scrubbing invariant is testable without a live
  // collector; defaults to the real session log in production.
  const rawLog = options.logText ?? sessionLogCollector.exportText();
  const logTail = buildScrubbedLogTail(rawLog, registry);
  const document = logTail.trim()
    ? `${diagnostics}\n\n${FEEDBACK_LOG_SECTION_HEADING}\n${logTail}\n`
    : `${diagnostics}\n`;
  const blob = new Blob([document], { type: FEEDBACK_DIAGNOSTICS_TYPE });
  if (blob.size > FEEDBACK_MAX_ATTACHMENT_BYTES) throw new Error('Diagnostics attachment exceeds its size limit.');
  return blob;
}

export function sanitizeFeedbackMessage(message: string, registry: SensitiveStringRegistry): string {
  return normalizedBoundedText(registry.scrubForReporting(message), FEEDBACK_MAX_MESSAGE_CHARS);
}

export const userDispatchAdapter: FeedbackAdapter = {
  async submit(publicKey, data) {
    // Keep the SDK out of the startup path entirely: when the public key is
    // unset or invalid, neither its module nor a provider client is loaded.
    const { UserDispatchClient } = await import('@userdispatch/sdk');
    const client = new UserDispatchClient({ apiKey: publicKey });
    await client.submit(data);
  },
};

export type SubmitFeedbackReportOptions = {
  publicKey: string | null | undefined;
  getState: () => FeedbackRuntimeState;
  isCurrent: () => boolean;
  includeDiagnostics: boolean;
  message: string;
  registry: SensitiveStringRegistry;
  eventCodes: readonly string[];
  adapter: FeedbackAdapter;
};

/**
 * The only SDK boundary. Tests read the Blob passed to `adapter` rather than
 * trusting a pre-serialization object inspection.
 */
export async function submitFeedbackReport(options: SubmitFeedbackReportOptions): Promise<boolean> {
  if (!isValidUserDispatchPublicKey(options.publicKey) || !options.isCurrent()) return false;
  const firstState = options.getState();
  if (firstState.sharing || firstState.screenSharing) return false;
  const cleanMessage = sanitizeFeedbackMessage(options.message, options.registry);
  if (!cleanMessage) return false;

  const attachment = options.includeDiagnostics
    ? buildFeedbackAttachment(firstState, options.eventCodes, options.registry)
    : null;
  const finalState = options.getState();
  if (!options.isCurrent() || finalState.sharing || finalState.screenSharing) return false;
  await options.adapter.submit(options.publicKey, {
    type: 'feedback',
    subject: 'Petal feedback',
    message: cleanMessage,
    metadata: { schema_version: DIAGNOSTICS_SCHEMA_VERSION },
    ...(attachment
      ? { files: [{ name: FEEDBACK_DIAGNOSTICS_FILENAME, content: attachment, type: FEEDBACK_DIAGNOSTICS_TYPE }] }
      : {}),
  });
  return true;
}

type FeedbackDom = {
  homeTrigger: HTMLButtonElement;
  meetingTrigger: HTMLButtonElement;
  dialog: HTMLDialogElement;
  form: HTMLFormElement;
  message: HTMLTextAreaElement;
  consent: HTMLInputElement;
  submit: HTMLButtonElement;
  cancel: HTMLButtonElement;
  status: HTMLElement;
  shareReason: HTMLElement;
};

export type FeedbackReportControllerOptions = {
  publicKey: string | null | undefined;
  dom: FeedbackDom;
  getState: () => FeedbackRuntimeState;
  registry: SensitiveStringRegistry;
  adapter?: FeedbackAdapter;
};

/** Petal-owned DOM dialog; no third-party widget/script is ever loaded. */
export class FeedbackReportController {
  private readonly options: FeedbackReportControllerOptions;
  private readonly publicKey: string | null;
  private readonly adapter: FeedbackAdapter;
  private readonly events: FeedbackEventCode[] = [];
  private epoch = 0;
  private shareIntentActive = false;
  private lastTrigger: HTMLButtonElement | null = null;

  constructor(options: FeedbackReportControllerOptions) {
    this.options = options;
    this.publicKey = isValidUserDispatchPublicKey(options.publicKey) ? options.publicKey.trim() : null;
    this.adapter = options.adapter ?? userDispatchAdapter;
  }

  install(): void {
    const { homeTrigger, meetingTrigger, dialog, form, message, consent, cancel } = this.options.dom;
    if (!this.publicKey) {
      homeTrigger.hidden = true;
      meetingTrigger.hidden = true;
      dialog.remove();
      return;
    }
    // The static markup starts hidden so an unconfigured deployment never
    // flashes the feature. Reveal it only after the public-key gate passes.
    homeTrigger.hidden = false;
    meetingTrigger.hidden = false;
    homeTrigger.addEventListener('click', () => this.open(homeTrigger));
    meetingTrigger.addEventListener('click', () => this.open(meetingTrigger));
    cancel.addEventListener('click', () => this.close());
    dialog.addEventListener('cancel', (event) => {
      event.preventDefault();
      this.close();
    });
    dialog.addEventListener('close', () => this.clearForm());
    message.addEventListener('input', () => this.refreshSubmit());
    consent.addEventListener('change', () => this.refreshSubmit());
    form.addEventListener('submit', (event) => {
      event.preventDefault();
      void this.submit();
    });
    this.refreshAvailability();
  }

  refreshAvailability(): void {
    if (!this.publicKey) return;
    const blocked = this.isShareActive();
    const { homeTrigger, meetingTrigger, shareReason } = this.options.dom;
    // #895: aria-disabled + a `blocked` class, not the `disabled` property --
    // a disabled control never fires hover/focus, so it could never reveal
    // the reason tooltip to a mouse or keyboard user. Safe regardless: click
    // still refuses via isShareActive() in open() below, and submission is
    // guarded three more times (submitFeedbackReport, refreshSubmit).
    for (const trigger of [homeTrigger, meetingTrigger]) {
      trigger.disabled = false;
      trigger.setAttribute('aria-disabled', blocked ? 'true' : 'false');
      trigger.classList.toggle('blocked', blocked);
      trigger.setAttribute('aria-describedby', blocked ? shareReason.id : '');
    }
    shareReason.hidden = !blocked;
    if (blocked) this.close();
    this.refreshSubmit();
  }

  /** Call synchronously before a picker or local-share publication begins. */
  onShareStartIntent(): void {
    this.epoch += 1;
    this.shareIntentActive = true;
    this.close();
    this.refreshAvailability();
  }

  onShareEnded(): void {
    this.shareIntentActive = false;
    this.refreshAvailability();
  }

  onDisconnect(): void {
    this.epoch += 1;
    this.shareIntentActive = false;
    this.close();
    this.refreshAvailability();
  }

  private open(trigger: HTMLButtonElement): void {
    if (!this.publicKey || this.isShareActive()) return;
    this.lastTrigger = trigger;
    this.events.push('feedback_opened');
    this.options.dom.status.textContent = '';
    this.options.dom.dialog.showModal();
    this.options.dom.message.focus();
    this.refreshSubmit();
  }

  private close(): void {
    this.epoch += 1;
    if (this.options.dom.dialog.open) this.options.dom.dialog.close();
    this.clearForm();
    this.lastTrigger?.focus();
  }

  private clearForm(): void {
    const { message, consent, submit } = this.options.dom;
    message.value = '';
    consent.checked = false;
    submit.disabled = true;
  }

  private isShareActive(): boolean {
    const state = this.options.getState();
    return this.shareIntentActive || state.sharing || state.screenSharing;
  }

  private refreshSubmit(): void {
    const { message, submit } = this.options.dom;
    const state = this.options.getState();
    submit.disabled =
      !this.publicKey ||
      state.sharing ||
      state.screenSharing ||
      normalizedBoundedText(message.value, FEEDBACK_MAX_MESSAGE_CHARS).length === 0;
  }

  private canSubmit(epoch: number): boolean {
    const state = this.options.getState();
    return Boolean(this.publicKey && epoch === this.epoch && !state.sharing && !state.screenSharing);
  }

  private async submit(): Promise<void> {
    const { message, consent, submit, status } = this.options.dom;
    const epoch = this.epoch;
    if (!this.canSubmit(epoch)) return;

    submit.disabled = true;
    status.textContent = '';

    try {
      // The state/epoch checks intentionally bracket Blob creation, then the
      // provider call: starting a share at either boundary cancels submission.
      const sent = await submitFeedbackReport({
        publicKey: this.publicKey,
        getState: this.options.getState,
        isCurrent: () => this.canSubmit(epoch),
        includeDiagnostics: consent.checked,
        message: message.value,
        registry: this.options.registry,
        eventCodes: [...this.events, 'feedback_submitted'],
        adapter: this.adapter,
      });
      if (!sent) return;
      if (!this.canSubmit(epoch)) return;
      status.textContent = 'Feedback sent.';
      message.value = '';
      consent.checked = false;
      this.refreshSubmit();
    } catch {
      if (this.canSubmit(epoch)) {
        status.textContent = 'Could not send feedback. Please try again later.';
        this.refreshSubmit();
      }
    }
  }
}
