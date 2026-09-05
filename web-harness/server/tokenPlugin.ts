// Vite dev-server plugin: mints real LiveKit JWTs via a local middleware
// route, so the web harness never needs a second process/port and the
// browser never sees LIVEKIT_API_SECRET.
//
// Uses the official `livekit-server-sdk` npm package's AccessToken class
// (NOT a hand-rolled JWT) -- same idea as
// apps/desktop/src-tauri/src/transport/token.rs's mint_access_token, just
// the JS/npm equivalent, wired into this dev server instead of a Rust CLI.
//
// Security note: this file runs ONLY in the Vite dev server's Node process.
// It reads LIVEKIT_API_KEY/LIVEKIT_API_SECRET from apps/desktop/.env via
// dotenv and holds them in server-side memory only -- they are read here,
// used here to sign a JWT, and the JWT (not the secret) is the only thing
// that ever reaches client JS via the HTTP response body. Never log the
// actual secret values, only whether they loaded.
import type { Plugin, ViteDevServer } from 'vite';
import type { IncomingMessage, ServerResponse } from 'node:http';
import { config as loadDotenv } from 'dotenv';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { AccessToken } from 'livekit-server-sdk';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// Loaded from apps/desktop/.env specifically (not web-harness/.env) so this
// harness reuses the exact same LiveKit Cloud/dev-server credentials the
// native Tauri app and its mint_token example already use -- one source of
// truth for local LiveKit config across the whole repo.
const ENV_PATH = path.resolve(__dirname, '..', '..', 'apps', 'desktop', '.env');

interface TokenRequestQuery {
  room?: string;
  identity?: string;
  displayName?: string;
  canPublish?: boolean;
  canSubscribe?: boolean;
  canPublishData?: boolean;
  hidden?: boolean;
}

interface LiveKitCredentials {
  url: string;
  apiKey: string;
  apiSecret: string;
}

interface TokenEndpointExposure {
  livekitUrl: string;
  apiKey: string;
  apiSecret: string;
  serverHost?: string | boolean;
  requestHost?: string;
  allowUnsafeNonLoopback?: boolean;
}

export const UNSAFE_TOKEN_ENDPOINT_ENV = 'PETAL_WEB_HARNESS_ALLOW_UNSAFE_TOKEN_ENDPOINT';

export const SAFE_TEST_GRANT = Object.freeze({
  roomJoin: true,
  canPublish: true,
  canSubscribe: true,
  canPublishData: true,
  canUpdateOwnMetadata: true,
  hidden: false,
});

function parseBooleanParam(value: string | null): boolean | undefined {
  if (value === null) return undefined;
  const normalized = value.toLowerCase();
  if (normalized === 'true' || normalized === '1') return true;
  if (normalized === 'false' || normalized === '0') return false;
  return undefined;
}

export function parseQuery(url: string): TokenRequestQuery {
  const parsed = new URL(url, 'http://localhost');
  return {
    room: parsed.searchParams.get('room') ?? undefined,
    identity: parsed.searchParams.get('identity') ?? undefined,
    displayName: parsed.searchParams.get('displayName') ?? undefined,
    canPublish: parseBooleanParam(parsed.searchParams.get('canPublish')),
    canSubscribe: parseBooleanParam(parsed.searchParams.get('canSubscribe')),
    canPublishData: parseBooleanParam(parsed.searchParams.get('canPublishData')),
    hidden: parseBooleanParam(parsed.searchParams.get('hidden')),
  };
}

function slugify(input: string): string {
  const slug = input
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .replace(/-{2,}/g, '-');
  return slug || 'room';
}

function livekitRoomName(meetingCode: string): string {
  return `petal-room-${slugify(meetingCode)}`;
}

async function readJsonBody(req: IncomingMessage): Promise<Partial<TokenRequestQuery>> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) {
    chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  if (chunks.length === 0) return {};
  return JSON.parse(Buffer.concat(chunks).toString('utf8')) as Partial<TokenRequestQuery>;
}

function sendJson(res: ServerResponse, status: number, body: unknown): void {
  const payload = JSON.stringify(body);
  res.statusCode = status;
  res.setHeader('Content-Type', 'application/json');
  res.setHeader('Content-Length', Buffer.byteLength(payload));
  res.end(payload);
}

function hostWithoutPort(host: string): string {
  const trimmed = host.trim().toLowerCase();
  if (trimmed.startsWith('[')) {
    const end = trimmed.indexOf(']');
    return end >= 0 ? trimmed.slice(1, end) : trimmed;
  }
  const colonIndex = trimmed.indexOf(':');
  return colonIndex >= 0 ? trimmed.slice(0, colonIndex) : trimmed;
}

export function isLoopbackHost(host: string | undefined): boolean {
  if (!host) return false;
  const hostname = hostWithoutPort(host);
  return (
    hostname === 'localhost' ||
    hostname === '::1' ||
    hostname === '0:0:0:0:0:0:0:1' ||
    /^127(?:\.\d{1,3}){3}$/.test(hostname)
  );
}

function isLoopbackUrl(url: string): boolean {
  try {
    return isLoopbackHost(new URL(url).hostname);
  } catch {
    return false;
  }
}

function serverHostIsLoopbackOnly(serverHost: string | boolean | undefined): boolean {
  if (serverHost === undefined || serverHost === false) return true;
  if (serverHost === true) return false;
  return isLoopbackHost(serverHost);
}

function usesLocalDevLiveKitCredentials({ url, apiKey, apiSecret }: LiveKitCredentials): boolean {
  return isLoopbackUrl(url) && apiKey === 'devkey' && apiSecret === 'secret';
}

export function tokenEndpointExposureError(exposure: TokenEndpointExposure): string | null {
  if (exposure.allowUnsafeNonLoopback) return null;
  if (usesLocalDevLiveKitCredentials({
    url: exposure.livekitUrl,
    apiKey: exposure.apiKey,
    apiSecret: exposure.apiSecret,
  })) {
    return null;
  }
  if (serverHostIsLoopbackOnly(exposure.serverHost) && isLoopbackHost(exposure.requestHost)) {
    return null;
  }
  return (
    'Refusing to mint LiveKit tokens with real credentials from a non-loopback ' +
    `web-harness dev server. Bind Vite to localhost or set ${UNSAFE_TOKEN_ENDPOINT_ENV}=1 ` +
    'only for an intentional private test network.'
  );
}

export function requestAsksForHidden(request: Partial<TokenRequestQuery> | Record<string, unknown>): boolean {
  const hidden = (request as Record<string, unknown>).hidden;
  return (
    hidden === true ||
    hidden === 1 ||
    (typeof hidden === 'string' && ['true', '1'].includes(hidden.toLowerCase()))
  );
}

export function safeGrantForRoom(room: string, request: Partial<TokenRequestQuery>) {
  if (requestAsksForHidden(request)) {
    throw new Error('hidden LiveKit participants are not allowed by the web-harness token endpoint');
  }
  return {
    ...SAFE_TEST_GRANT,
    room,
  };
}

/**
 * Mints a room-join LiveKit JWT for `identity` in `room` with both publish
 * and subscribe grants (the harness always needs both -- it publishes a
 * synthetic window share and renders every other participant's tracks).
 */
export async function mintToken(
  apiKey: string,
  apiSecret: string,
  room: string,
  identity: string,
  request: Partial<TokenRequestQuery>,
): Promise<string> {
  const at = new AccessToken(apiKey, apiSecret, {
    identity,
    name: request.displayName ?? identity,
    ttl: '24h',
  });
  at.addGrant(safeGrantForRoom(room, request));
  return at.toJwt();
}

export function tokenEndpointPlugin(): Plugin {
  return {
    name: 'petal-web-harness-token-endpoint',
    configureServer(server: ViteDevServer) {
      // Load apps/desktop/.env once when the dev server starts. Only report
      // whether each variable loaded -- never the actual secret value.
      const result = loadDotenv({ path: ENV_PATH });
      const loadedFromFile = !result.error;
      const url = process.env.LIVEKIT_URL;
      const apiKey = process.env.LIVEKIT_API_KEY;
      const apiSecret = process.env.LIVEKIT_API_SECRET;

      server.config.logger.info(
        `[token-endpoint] apps/desktop/.env ${loadedFromFile ? 'loaded' : 'NOT found'} ` +
          `(LIVEKIT_URL: ${url ? 'set' : 'MISSING'}, ` +
          `LIVEKIT_API_KEY: ${apiKey ? 'set' : 'MISSING'}, ` +
          `LIVEKIT_API_SECRET: ${apiSecret ? 'set' : 'MISSING'})`,
      );

      server.middlewares.use('/api/token', (req: IncomingMessage, res: ServerResponse) => {
        void (async () => {
          if (req.method !== 'GET' && req.method !== 'POST') {
            sendJson(res, 405, { error: 'method not allowed, use GET or POST' });
            return;
          }

          const currentUrl = process.env.LIVEKIT_URL;
          const currentKey = process.env.LIVEKIT_API_KEY;
          const currentSecret = process.env.LIVEKIT_API_SECRET;
          if (!currentUrl || !currentKey || !currentSecret) {
            sendJson(res, 500, {
              error:
                'LiveKit credentials not configured -- check apps/desktop/.env has ' +
                'LIVEKIT_URL / LIVEKIT_API_KEY / LIVEKIT_API_SECRET set.',
            });
            return;
          }

          const body = req.method === 'POST' ? await readJsonBody(req) : {};
          const query = parseQuery(req.url ?? '');
          const request = { ...query, ...body };
          const { room, identity } = request;
          if (!room || !identity) {
            sendJson(res, 400, { error: '"room" and "identity" are both required' });
            return;
          }
          if (requestAsksForHidden(request)) {
            sendJson(res, 400, {
              error: 'hidden LiveKit participants are not allowed by the web-harness token endpoint',
            });
            return;
          }

          const exposureError = tokenEndpointExposureError({
            livekitUrl: currentUrl,
            apiKey: currentKey,
            apiSecret: currentSecret,
            serverHost: server.config.server.host,
            requestHost: req.headers.host,
            allowUnsafeNonLoopback: process.env[UNSAFE_TOKEN_ENDPOINT_ENV] === '1',
          });
          if (exposureError) {
            sendJson(res, 403, { error: exposureError });
            return;
          }

          try {
            const livekitRoom = livekitRoomName(room);
            const token = await mintToken(currentKey, currentSecret, livekitRoom, identity, request);
            sendJson(res, 200, { url: currentUrl, token, room: livekitRoom });
          } catch (err) {
            sendJson(res, 500, { error: `token minting failed: ${(err as Error).message ?? err}` });
          }
        })();
      });
    },
  };
}
