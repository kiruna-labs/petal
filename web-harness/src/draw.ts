import { DRAW_TOPIC, type DrawMessage, type DrawPoint } from './trackNames.ts';

const DRAW_TYPES = new Set(['begin', 'points', 'end', 'clear', 'text']);
export const MAX_DRAW_TEXT_CHARS = 256;

function validDrawText(text: unknown): text is string {
  return (
    typeof text === 'string' &&
    text.trim().length > 0 &&
    [...text].length <= MAX_DRAW_TEXT_CHARS &&
    !/[\n\r\u2028\u2029]/u.test(text)
  );
}

export function drawPublishOptions(): { reliable: boolean; topic: typeof DRAW_TOPIC } {
  return { reliable: true, topic: DRAW_TOPIC };
}

export function parseDrawPayload(payload: Uint8Array | string): DrawMessage | null {
  let text: string;
  try {
    text = typeof payload === 'string' ? payload : new TextDecoder().decode(payload);
  } catch {
    return null;
  }

  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return null;
  }

  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return null;
  const candidate = raw as Partial<DrawMessage> & { text?: unknown };
  if (
    candidate.v !== 1 ||
    typeof candidate.type !== 'string' ||
    !DRAW_TYPES.has(candidate.type) ||
    typeof candidate.ownerIdentity !== 'string' ||
    typeof candidate.windowId !== 'number' ||
    !Number.isSafeInteger(candidate.windowId) ||
    candidate.windowId < 1 ||
    candidate.windowId > 0xffff_ffff ||
    typeof candidate.seq !== 'number' ||
    !Number.isSafeInteger(candidate.seq) ||
    candidate.seq < 0 ||
    !Array.isArray(candidate.points)
  ) {
    return null;
  }

  const ownerIdentity = candidate.ownerIdentity.trim();
  if (!ownerIdentity) return null;

  const points = parseDrawPoints(candidate.points);
  if (!points) return null;

  if (candidate.type === 'clear') {
    if (candidate.strokeId !== null || points.length !== 0 || candidate.text !== undefined) return null;
    return {
      v: 1,
      type: 'clear',
      ownerIdentity,
      windowId: candidate.windowId,
      seq: candidate.seq,
      strokeId: null,
      points: [],
    };
  }

  if (typeof candidate.strokeId !== 'string' || !candidate.strokeId.trim()) return null;
  if (candidate.type === 'text') {
    if (points.length !== 1 || !validDrawText(candidate.text)) return null;
    return {
      v: 1,
      type: 'text',
      ownerIdentity,
      windowId: candidate.windowId,
      seq: candidate.seq,
      strokeId: candidate.strokeId.trim(),
      points: [points[0]],
      text: candidate.text,
    };
  }
  if (candidate.text !== undefined || (candidate.type !== 'end' && points.length === 0)) return null;
  return {
    v: 1,
    type: candidate.type,
    ownerIdentity,
    windowId: candidate.windowId,
    seq: candidate.seq,
    strokeId: candidate.strokeId.trim(),
    points,
  };
}

function parseDrawPoints(rawPoints: unknown[]): DrawPoint[] | null {
  const points: DrawPoint[] = [];
  for (const rawPoint of rawPoints) {
    if (!rawPoint || typeof rawPoint !== 'object' || Array.isArray(rawPoint)) return null;
    const point = rawPoint as Partial<DrawPoint>;
    if (
      typeof point.x !== 'number' ||
      typeof point.y !== 'number' ||
      !Number.isFinite(point.x) ||
      !Number.isFinite(point.y) ||
      point.x < 0 ||
      point.x > 1 ||
      point.y < 0 ||
      point.y > 1
    ) {
      return null;
    }
    points.push({ x: point.x, y: point.y });
  }
  return points;
}
