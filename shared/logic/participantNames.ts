// SINGLE SOURCE OF TRUTH for turning raw LiveKit participant `name` values
// into something a human can read (#122). Consumed by the desktop main-menu
// room-row roster tooltip (apps/desktop/src/lib/components/RoomRow.svelte, via
// $lib/data/participantNames.ts) and by the web client's tile labels
// (web-harness/src/tiles.ts re-exports `looksLikeTechnicalIdentity` from here
// rather than keeping its own copy), so the two surfaces cannot drift on what
// counts as "not a real name".
//
// A participant `name` is minted from the client's `displayName` and falls
// back to the raw identity (backend/lib/livekit.ts), so it is routinely a
// UUID, a `web-<uuid>`, or empty. Never render one of those.

/**
 * Whether this string is an opaque machine identity rather than a name a
 * person chose. Moved here verbatim from web-harness/src/tiles.ts; the three
 * shapes are the identity formats `handlers.ts` mints and accepts.
 */
export function looksLikeTechnicalIdentity(value: string): boolean {
  const trimmed = value.trim();
  return (
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(trimmed) ||
    /^web-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(trimmed) ||
    /^[0-9a-f]{32}$/i.test(trimmed)
  );
}

/**
 * The desktop's stand-in for a name it cannot show. Matches
 * `RemoteWindowHeader.svelte`'s existing wording for a blank owner name; the
 * web client says "Guest" for the same condition on its own tiles.
 */
export const UNNAMED_PARTICIPANT = 'Someone';

/**
 * One participant's rendered label. An empty name and a machine identity both
 * collapse to `UNNAMED_PARTICIPANT` -- the room-status endpoint deliberately
 * never sends identities (#122), so there is nothing else to fall back to.
 */
export function participantNameLabel(name: string | null | undefined): string {
  const trimmed = (name ?? '').trim();
  if (!trimmed) return UNNAMED_PARTICIPANT;
  if (looksLikeTechnicalIdentity(trimmed)) return UNNAMED_PARTICIPANT;
  return trimmed;
}

/** How many names a summary spells out before it starts counting. */
export const PARTICIPANT_SUMMARY_MAX = 3;

/**
 * "Who is in this room", as one line: `A`, `A and B`, `A, B and C`,
 * `A, B, C and 30 more`. Returns `''` for an empty roster so the caller can
 * decide whether to render anything at all.
 *
 * Nothing here truncates: the caller renders the FULL string and must let it
 * wrap (CLAUDE.md's "UI text must NEVER truncate" rule). `max` bounds how many
 * names are spelled out, never how wide the result is allowed to be.
 */
export function participantNamesSummary(
  names: readonly (string | null | undefined)[],
  max: number = PARTICIPANT_SUMMARY_MAX
): string {
  const labels = names.map(participantNameLabel);
  if (labels.length === 0) return '';
  const cap = Math.max(1, Math.floor(max));
  if (labels.length > cap) {
    const remaining = labels.length - cap;
    return `${labels.slice(0, cap).join(', ')} and ${remaining} more`;
  }
  if (labels.length === 1) return labels[0]!;
  return `${labels.slice(0, -1).join(', ')} and ${labels[labels.length - 1]!}`;
}
