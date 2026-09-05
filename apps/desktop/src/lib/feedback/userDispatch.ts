// Bundled UserDispatch SDK adapter (#292). No hosted widget/script is ever
// loaded -- `@userdispatch/sdk` is a normal npm dependency, dynamically
// imported ONLY from inside `submitFeedback` below, so a build with no
// public key configured (`userDispatchPublicKey()` returns `null`, see
// `config.ts`) never even fetches/evaluates the SDK module.

import { invoke } from '@tauri-apps/api/core';
import { COMMANDS, type FeedbackDiagnostics } from '$lib/ipc';
import { userDispatchPublicKey } from './config';
import { normalizedFeedbackMessage, FEEDBACK_MAX_MESSAGE_CHARS } from './messageSanitizer';

export { normalizedFeedbackMessage, FEEDBACK_MAX_MESSAGE_CHARS };

export interface PreparedDiagnosticsAttachment {
  filename: string;
  mimeType: string;
  blob: Blob;
  byteCount: number;
}

function base64ToBlob(base64: string, mimeType: string): Blob {
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) {
    bytes[i] = binary.charCodeAt(i);
  }
  return new Blob([bytes], { type: mimeType });
}

/**
 * Asks the native side to build the bounded, redacted diagnostic zip
 * (`src-tauri/src/feedback.rs::prepare_feedback_diagnostics`). Rejects with
 * `'sharing_active'` if a share is in progress -- callers should surface a
 * "submit without diagnostics" fallback rather than retrying silently.
 */
export async function prepareDiagnosticsAttachment(): Promise<PreparedDiagnosticsAttachment> {
  const result = await invoke<FeedbackDiagnostics>(COMMANDS.prepareFeedbackDiagnostics);
  return {
    filename: result.filename,
    mimeType: result.mimeType,
    blob: base64ToBlob(result.bytesBase64, result.mimeType),
    byteCount: result.byteCount
  };
}

export interface SubmitFeedbackOptions {
  message: string;
  attachment?: PreparedDiagnosticsAttachment | null;
}

/**
 * The only UserDispatch network boundary in this app. Sends exactly the
 * public key, a fixed subject, the sanitized message, and (only when the
 * caller opted in and preparation succeeded) one attachment -- no room
 * name, identity, join URL, or other session metadata is ever added to the
 * payload. Throws on failure; callers must not log the error content
 * (may echo back user-typed text) -- surface a generic message instead.
 */
export async function submitFeedback(options: SubmitFeedbackOptions): Promise<void> {
  const publicKey = userDispatchPublicKey();
  if (!publicKey) throw new Error('feedback is not configured for this build');

  const message = normalizedFeedbackMessage(options.message);
  if (!message) throw new Error('feedback message is empty');

  // Dynamic import: keeps the SDK entirely out of the startup bundle graph
  // evaluation for every build that doesn't configure a public key.
  const { UserDispatchClient } = await import('@userdispatch/sdk');
  const client = new UserDispatchClient({ apiKey: publicKey });

  await client.submit({
    type: 'feedback',
    subject: 'Petal feedback',
    message,
    ...(options.attachment
      ? {
          files: [
            {
              name: options.attachment.filename,
              content: options.attachment.blob,
              type: options.attachment.mimeType
            }
          ]
        }
      : {})
  });
}
