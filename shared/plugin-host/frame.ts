// Builds the sandboxed iframe a plugin runs in. One code path for both
// clients. The frame gets: `sandbox="allow-scripts"` (opaque origin, no
// same-origin, no popups, no forms, no top navigation), a `<meta>` CSP that
// forbids every network fetch, and a srcdoc containing the frame runtime
// followed by the plugin's own module. Design: plugins/README.md §2.3.
//
// Desktop note: Tauri's IPC init scripts are injected into the main frame
// only, and the IPC endpoint rejects the `null` origin a sandboxed frame has,
// so `__TAURI_INTERNALS__` is undefined inside. A rendered test asserts this.

import { FRAME_RUNTIME_SOURCE } from './frameRuntime.ts';

export const PLUGIN_FRAME_CSP =
  "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; " +
  "img-src data: blob:; font-src data:; connect-src 'none'; frame-src 'none'; " +
  "form-action 'none'; base-uri 'none'";

export const PLUGIN_FRAME_SANDBOX = 'allow-scripts';

/**
 * Make arbitrary JS safe to inline inside a `<script>` element. `<\/` is a
 * valid escape in strings, regexes, template literals and comments alike, so
 * the program's meaning is unchanged; the HTML parser just cannot see a
 * closing tag any more. `<!--` gets the same treatment (HTML comment start).
 */
export function escapeInlineScript(source: string): string {
  return source.replace(/<\/(script)/gi, '<\\/$1').replace(/<!--/g, '<\\!--');
}

export interface FrameSrcdocOptions {
  pluginId: string;
  /** The plugin's single-file ES module (bundle `files["plugin.js"]`). */
  source: string;
  /** UI surface frames get a body the plugin can draw into; logic frames stay empty. */
  surface?: boolean;
  /** Override for tests. */
  runtime?: string;
}

export function buildPluginFrameSrcdoc({ pluginId, source, surface = false, runtime = FRAME_RUNTIME_SOURCE }: FrameSrcdocOptions): string {
  const title = `Petal plugin ${pluginId}`.replace(/[<&"]/g, '');
  const bodyStyle = surface
    ? 'margin:0;background:transparent;color-scheme:dark;font:14px system-ui,sans-serif;'
    : 'margin:0;display:none;';
  return (
    '<!doctype html><html><head><meta charset="utf-8">' +
    `<meta http-equiv="Content-Security-Policy" content="${PLUGIN_FRAME_CSP}">` +
    `<title>${title}</title>` +
    `<style>html,body{${bodyStyle}}</style>` +
    '</head><body>' +
    `<script>${escapeInlineScript(runtime)}</script>` +
    `<script type="module">${escapeInlineScript(source)}</script>` +
    '</body></html>'
  );
}

export interface CreateFrameOptions extends FrameSrcdocOptions {
  /** Applied to the element; surface frames position themselves via the host. */
  className?: string;
  title?: string;
}

/** Create (but do not attach) a fully locked-down plugin iframe. */
export function createPluginFrame(doc: Document, opts: CreateFrameOptions): HTMLIFrameElement {
  const frame = doc.createElement('iframe');
  // Sandbox MUST be set before any content so the first document is already sandboxed.
  frame.setAttribute('sandbox', PLUGIN_FRAME_SANDBOX);
  frame.setAttribute('referrerpolicy', 'no-referrer');
  frame.setAttribute('title', opts.title ?? `Plugin ${opts.pluginId}`);
  frame.setAttribute('data-plugin-id', opts.pluginId);
  frame.setAttribute('data-plugin-frame', opts.surface ? 'surface' : 'logic');
  if (opts.className) frame.className = opts.className;
  if (!opts.surface) {
    frame.hidden = true;
    frame.setAttribute('aria-hidden', 'true');
    frame.tabIndex = -1;
  }
  frame.srcdoc = buildPluginFrameSrcdoc(opts);
  return frame;
}
