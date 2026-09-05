const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;
const GENERATED_PARTICIPANT_RE = /^p-[a-z0-9]+-[a-z0-9]+$/i;
const LONG_HEX_RE = /^[0-9a-f]{20,}$/i;

function clean(value: string | null | undefined): string | null {
  const cleaned = value?.replace(/\s+/g, ' ').trim();
  return cleaned ? cleaned : null;
}

export function isTechnicalIdentity(value: string | null | undefined): boolean {
  const id = clean(value);
  if (!id) return true;
  return UUID_RE.test(id) || GENERATED_PARTICIPANT_RE.test(id) || LONG_HEX_RE.test(id);
}

function readableIdentity(value: string): string {
  return value
    .replace(/[_-]+/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .replace(/\b\w/g, (char) => char.toUpperCase());
}

export function friendlyTelepointerName(
  displayName: string | null | undefined,
  userId: string | null | undefined,
  fallback = 'Guest'
): string {
  const name = clean(displayName);
  if (name) return name;

  const id = clean(userId);
  if (!id || isTechnicalIdentity(id)) return fallback;

  return readableIdentity(id) || fallback;
}
