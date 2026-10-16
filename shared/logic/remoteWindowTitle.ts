// SINGLE SOURCE OF TRUTH for the label a remote window's header shows (#318).
// Consumed by the desktop app (apps/desktop/src/lib/ipc.ts re-exports it) and
// the web client (web-harness/src/remoteWindowHeader.ts imports it directly),
// so the two headers can never disagree on what a shared window is called.
//
// User directive (2026-10-15): remote window titles are shortened and never
// break onto a second line. The header shows the document part of the title;
// the full, unshortened title stays in the tooltip.

const RAW_WINDOW_TRACK_RE = /^petal-window(?:-\d+|-capture)?$/i;
const RAW_CAMERA_TRACK_RE = /^petal-camera(?:-.+)?$/i;

// " — " (macOS composition, `session/share.rs` "{title} — {app}"), " – ", and
// " - " (Windows: "*Untitled - Notepad"). Whitespace is required on the left so
// hyphenated words ("well-known-file.txt") are never split; a trailing dash at
// the very end ("Untitled —") also counts.
const SUFFIX_SEPARATOR_RE = /\s+[—–-](?:\s+|$)/g;

function isRawRemoteTrackName(label: string): boolean {
  return RAW_WINDOW_TRACK_RE.test(label) || RAW_CAMERA_TRACK_RE.test(label);
}

/** Splits off the LAST " — App" / " – App" / " - App" segment, if any. */
function splitLastSuffix(title: string): { head: string; suffix: string; separator: string } | null {
  let last: RegExpExecArray | null = null;
  for (const match of title.matchAll(SUFFIX_SEPARATOR_RE)) last = match as RegExpExecArray;
  if (!last || last.index === undefined) return null;
  const head = title.slice(0, last.index).trim();
  if (!head) return null;
  return {
    head,
    suffix: title.slice(last.index + last[0].length).trim(),
    separator: last[0].trim()
  };
}

/**
 * Drops the application suffix from a window title, keeping the document part:
 * "Spec — Safari" -> "Spec", "*Untitled - Notepad" -> "*Untitled". A title that
 * is only an app name, or whose document part would be empty, is returned
 * as-is (trimmed). When Petal's own " — App" composition wraps a title that
 * already ends in that app's name ("Doc - Visual Studio Code — Code"), both
 * suffixes go.
 */
export function shortenWindowTitle(title: string): string {
  const trimmed = title.trim();
  const first = splitLastSuffix(trimmed);
  if (!first) return trimmed;
  if (first.separator !== '—' || !first.suffix) return first.head;
  const second = splitLastSuffix(first.head);
  if (!second || second.separator === '—' || !second.suffix) return first.head;
  const app = first.suffix.toLowerCase();
  const inner = second.suffix.toLowerCase();
  return endsWithWord(inner, app) || endsWithWord(app, inner) ? second.head : first.head;
}

/** "visual studio code" ends with the word "code"; "data" does not end with "a". */
function endsWithWord(text: string, word: string): boolean {
  return text === word || text.endsWith(` ${word}`);
}

/**
 * The header label for a remote window's source title: the shortened title,
 * with Petal's raw track names ("petal-window-57", "petal-camera-bob") mapped
 * to "Shared window" / "Camera" and never shown to the user.
 */
export function remoteWindowSourceLabel(sourceTitle: string): string {
  const trimmed = sourceTitle.trim();
  if (!trimmed || RAW_WINDOW_TRACK_RE.test(trimmed)) return 'Shared window';
  if (RAW_CAMERA_TRACK_RE.test(trimmed)) return 'Camera';
  const shortened = shortenWindowTitle(trimmed);
  if (!isRawRemoteTrackName(shortened)) return shortened;
  // "petal-window-57 — Sublime": the document part is a raw track name, so
  // the app name is the only human label available.
  const parts = trimmed.split(/\s+[—–-](?:\s+|$)/).map((part) => part.trim());
  const human = [...parts].reverse().find((part) => part && !isRawRemoteTrackName(part));
  if (human) return human;
  return RAW_CAMERA_TRACK_RE.test(shortened) ? 'Camera' : 'Shared window';
}

/**
 * The full source title for the header's tooltip -- not stripped of its app
 * suffix, so a shortened or ellipsized header label never hides information.
 * Raw track names still map to their human label.
 */
export function remoteWindowFullSourceLabel(sourceTitle: string): string {
  const trimmed = sourceTitle.trim();
  const label = remoteWindowSourceLabel(trimmed);
  return label === shortenWindowTitle(trimmed) ? trimmed : label;
}
