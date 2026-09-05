// CORS helper for api/j.ts, copied (trimmed) from backend/lib/http.ts — only
// the parts j.ts needs. See that file for the full version used by the
// token/rooms/admin/etc. endpoints, which stay in the backend project.

import type { VercelRequest, VercelResponse } from '@vercel/node';

const DEFAULT_ALLOWED_ORIGINS = ['https://meet.petal.live'];

function headerValue(value: string | string[] | undefined): string | undefined {
  return Array.isArray(value) ? value[0] : value;
}

function configuredAllowedOrigins(): Set<string> {
  const configured = process.env.PETAL_ALLOWED_ORIGINS;
  const origins = configured
    ? configured
        .split(',')
        .map((origin) => origin.trim())
        .filter(Boolean)
    : DEFAULT_ALLOWED_ORIGINS;
  return new Set(origins);
}

function isAllowedOrigin(origin: string): boolean {
  if (/^https?:\/\/(localhost|127\.0\.0\.1)(:\d+)?$/.test(origin)) return true;
  return configuredAllowedOrigins().has(origin);
}

// Restrictive CORS for browser callers. Native app / server-to-server requests
// usually carry no Origin header, so they are allowed without emitting a
// wildcard. Returns true if the request was fully handled here.
export function applyCors(req: VercelRequest, res: VercelResponse): boolean {
  const origin = headerValue(req.headers.origin);
  if (origin) {
    if (!isAllowedOrigin(origin)) {
      res.status(403).json({ error: 'origin not allowed' });
      return true;
    }
    res.setHeader('Access-Control-Allow-Origin', origin);
    res.setHeader('Vary', 'Origin');
  }
  res.setHeader('Access-Control-Allow-Methods', 'GET, POST, OPTIONS');
  res.setHeader('Access-Control-Allow-Headers', 'Content-Type');
  if (req.method === 'OPTIONS') {
    res.status(204).end();
    return true;
  }
  return false;
}
