<!--
  FeedbackModal — Petal-owned "Send feedback" dialog (#292). Reuses the
  shared Modal.svelte shell (scrim, Escape, close affordance). No hosted
  UserDispatch widget/script is ever loaded here -- submission goes through
  the bundled `@userdispatch/sdk` adapter in `$lib/feedback/userDispatch.ts`,
  dynamically imported only at submit time.

  Privacy/safety properties (see #292's issue thread for the full history):
  - Log attachment is opt-in PER SUBMISSION: the checkbox always starts
    unchecked and is never remembered across opens.
  - Sharing guard: this modal auto-closes (and blocks opening) while ANY
    window is being shared -- `sharedWindowIds()` is polled while mounted,
    and rechecked immediately before both attachment preparation and the
    final submit call. Not a perfect atomic guard (see feedback.rs's doc
    comment on why), but closes the practical window without touching
    #298's exclusive session/share.rs lock.
  - Never logs the message text or submission errors to the console -- only
    a generic, user-facing status string.
  - A reply address is required (#245): format-checked by the shared rule,
    never verified, sent only as the SDK's `email` field, and remembered on
    this device (prefilled, editable) once a report goes out.
-->
<script lang="ts">
  import { onDestroy, onMount, tick } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import Modal from './Modal.svelte';
  import Button from './Button.svelte';
  import Checkbox from '@petal/shared/ui/components/Checkbox.svelte';
  import { COMMANDS } from '$lib/ipc';
  import { STORAGE_KEYS } from '$lib/data/storageKeys';
  import {
    feedbackEmailError,
    feedbackEmailStorage,
    isValidFeedbackEmail,
    rememberFeedbackEmail,
    rememberedFeedbackEmail
  } from '$lib/feedback/email';
  import {
    FEEDBACK_MAX_MESSAGE_CHARS,
    prepareDiagnosticsAttachment,
    submitFeedback,
    type PreparedDiagnosticsAttachment
  } from '$lib/feedback/userDispatch';

  interface Props {
    onClose: () => void;
  }

  let { onClose }: Props = $props();

  let message = $state('');
  let email = $state(rememberedFeedbackEmail(feedbackEmailStorage(), STORAGE_KEYS.feedbackEmail));
  // The inline error waits until the field has been left once.
  let emailTouched = $state(false);
  let attachDiagnostics = $state(false);
  let status = $state<'idle' | 'preparing' | 'submitting' | 'success' | 'error'>('idle');
  let statusMessage = $state<string | null>(null);
  let sharing = $state(false);
  let textareaEl: HTMLTextAreaElement | undefined = $state();
  let previousActiveElement: HTMLElement | null = null;
  let sharePoll: ReturnType<typeof setInterval> | undefined;
  let destroyed = false;

  const trimmedLength = $derived(message.trim().length);
  const busy = $derived(status === 'preparing' || status === 'submitting');
  const emailError = $derived(emailTouched ? feedbackEmailError(email) : null);
  const submitDisabled = $derived(busy || sharing || trimmedLength === 0 || !isValidFeedbackEmail(email));

  async function checkSharing(): Promise<boolean> {
    try {
      const ids = await invoke<number[]>(COMMANDS.sharedWindowIds);
      return ids.length > 0;
    } catch {
      // No Tauri bridge (browser preview) -- never block on a check that
      // can't run there.
      return false;
    }
  }

  async function refreshSharing() {
    const active = await checkSharing();
    if (destroyed) return;
    sharing = active;
    if (active) {
      // Sharing started while the modal was open -- close immediately
      // rather than let the user submit into a stale, no-longer-safe state.
      onClose();
    }
  }

  onMount(async () => {
    previousActiveElement = document.activeElement as HTMLElement | null;
    await refreshSharing();
    sharePoll = setInterval(() => void refreshSharing(), 2000);
    await tick();
    textareaEl?.focus();
  });

  onDestroy(() => {
    destroyed = true;
    if (sharePoll) clearInterval(sharePoll);
    previousActiveElement?.focus?.();
  });

  async function handleSubmit(event: SubmitEvent) {
    event.preventDefault();
    if (submitDisabled) return;

    statusMessage = null;

    // Final sharing recheck before doing any work (see module doc).
    if (await checkSharing()) {
      sharing = true;
      statusMessage = "Feedback isn't available while you're sharing a window.";
      status = 'error';
      return;
    }

    let attachment: PreparedDiagnosticsAttachment | null = null;
    if (attachDiagnostics) {
      status = 'preparing';
      try {
        attachment = await prepareDiagnosticsAttachment();
      } catch (e) {
        status = 'error';
        statusMessage =
          e === 'sharing_active' || (e instanceof Error && e.message === 'sharing_active')
            ? "Feedback isn't available while you're sharing a window."
            : 'Could not prepare the diagnostic attachment. You can uncheck it and submit without one.';
        return;
      }
    }

    // Recheck once more immediately before the network call -- a share
    // could have started during attachment preparation above.
    if (await checkSharing()) {
      sharing = true;
      statusMessage = "Feedback isn't available while you're sharing a window.";
      status = 'error';
      return;
    }

    status = 'submitting';
    const address = email;
    try {
      await submitFeedback({ message, email: address, attachment });
      // Remembered once it was actually sent, even if the modal has since
      // closed: next time the form opens with it filled in.
      rememberFeedbackEmail(feedbackEmailStorage(), STORAGE_KEYS.feedbackEmail, address);
      if (destroyed) return;
      status = 'success';
      statusMessage = 'Feedback sent. Thank you!';
      message = '';
      attachDiagnostics = false;
    } catch {
      if (destroyed) return;
      status = 'error';
      statusMessage = 'Could not send feedback. Please try again later.';
    }
  }
</script>

<Modal title="Send feedback" eyebrow="Petal" onClose={onClose} width="compact">
  {#if sharing}
    <p class="share-notice" role="status">
      Feedback isn't available while you're sharing a window. This will close automatically.
    </p>
  {:else}
    <!-- novalidate: the shared feedbackEmail.ts rule decides, not the webview's own type=email check. -->
    <form class="feedback-form" onsubmit={handleSubmit} novalidate>
      <label class="field">
        <span class="field-label">What's on your mind?</span>
        <textarea
          bind:this={textareaEl}
          bind:value={message}
          maxlength={FEEDBACK_MAX_MESSAGE_CHARS}
          placeholder="Tell us what's working, what's not, or what you'd like to see."
          rows="5"
          disabled={busy}
        ></textarea>
      </label>

      <div class="field">
        <label class="field-label" for="feedback-email">Your email</label>
        <input
          id="feedback-email"
          type="email"
          bind:value={email}
          required
          autocomplete="email"
          inputmode="email"
          autocapitalize="off"
          spellcheck="false"
          aria-invalid={emailError ? 'true' : 'false'}
          aria-describedby="feedback-email-error feedback-email-hint"
          disabled={busy}
          onblur={() => (emailTouched = true)}
          onkeydown={(event) => {
            // Send is disabled while the address is bad, so Enter would
            // otherwise do nothing at all: say why instead.
            if (event.key === 'Enter') emailTouched = true;
          }}
        />
        <!-- Always-rendered live region: only the error inside it toggles, so it is announced; the static hint never is. -->
        <div class="field-notes" aria-live="polite">
          <span id="feedback-email-error" class="field-error" hidden={!emailError}>{emailError ?? ''}</span>
          <span id="feedback-email-hint" class="field-hint">So we can reply. We won't verify it. Remembered on this device.</span>
        </div>
      </div>

      <label class="checkbox-row">
        <Checkbox bind:checked={attachDiagnostics} disabled={busy} />
        <span class="checkbox-copy">
          Attach a redacted diagnostic log
          <span class="checkbox-hint">
            A short, recent excerpt of your Petal log with room and identity values redacted. Only
            sent if you check this box and press Send.
          </span>
        </span>
      </label>

      {#if statusMessage}
        <!-- Errors interrupt (role=alert); success is a polite status. -->
        <p
          class="status-line"
          class:success={status === 'success'}
          class:error={status === 'error'}
          role={status === 'error' ? 'alert' : 'status'}
        >
          {statusMessage}
        </p>
      {/if}

      <p class="disclosure">
        Your message and email address are sent to UserDispatch, our feedback provider.
        <a href="https://userdispatch.com/privacy" target="_blank" rel="noreferrer">Privacy policy</a>
      </p>

      <div class="actions">
        <Button variant="ghost" onclick={onClose}>Cancel</Button>
        <Button variant="primary" type="submit" disabled={submitDisabled}>
          {status === 'preparing' ? 'Preparing…' : status === 'submitting' ? 'Sending…' : 'Send'}
        </Button>
      </div>
    </form>
  {/if}
</Modal>

<style>
  .feedback-form {
    display: flex;
    flex-direction: column;
    gap: 14px;
    padding: 16px 18px 18px;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .field-label {
    font: 600 12px var(--font-ui);
    color: var(--text-primary);
  }

  textarea {
    width: 100%;
    box-sizing: border-box;
    resize: vertical;
    min-height: 96px;
    border-radius: var(--radius-input);
    border: 1px solid var(--hairline);
    background: var(--surface);
    color: var(--text-primary);
    font: 400 12.5px var(--font-ui);
    padding: 10px 11px;
  }

  textarea:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 1px;
  }

  input[type='email'] {
    width: 100%;
    box-sizing: border-box;
    height: 34px;
    border-radius: var(--radius-input);
    border: 1px solid var(--hairline);
    background: var(--surface);
    color: var(--text-primary);
    font: 400 12.5px var(--font-ui);
    padding: 0 11px;
  }

  input[type='email']:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 1px;
  }

  input[type='email'][aria-invalid='true'] {
    border-color: var(--danger);
  }

  .field-notes {
    display: flex;
    flex-direction: column;
    gap: 3px;
  }

  .field-error {
    font: 600 11px var(--font-ui);
    color: var(--danger);
  }

  .field-hint {
    font: 400 11px var(--font-ui);
    color: var(--text-faint);
  }

  .checkbox-row {
    display: flex;
    align-items: flex-start;
    gap: 9px;
    cursor: pointer;
  }

  .checkbox-copy {
    display: flex;
    flex-direction: column;
    gap: 3px;
    font: 600 12px var(--font-ui);
    color: var(--text-primary);
  }

  .checkbox-hint {
    font: 400 11px var(--font-ui);
    color: var(--text-faint);
  }

  .disclosure {
    margin: 0;
    font: 400 10.5px var(--font-ui);
    color: var(--text-faint);
  }

  .disclosure a {
    color: var(--text-faint);
    text-decoration: underline;
  }

  .status-line {
    margin: 0;
    font: 600 11.5px var(--font-ui);
    color: var(--text-faint);
  }

  .status-line.success {
    color: var(--live);
  }

  .status-line.error {
    color: var(--danger);
  }

  .share-notice {
    margin: 0;
    padding: 16px 18px 18px;
    font: 500 12.5px var(--font-ui);
    color: var(--text-faint);
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 4px;
  }
</style>
