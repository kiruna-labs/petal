// Gemini Live ephemeral-token minting (#655). GEMINI_API_KEY lives ONLY in
// this backend's server env — never in a client, never in a response, never in
// a log line. Same posture as lib/livekit.ts's LIVEKIT_API_SECRET: Petal's
// repo is public, so the maintainers' key can only exist here.
//
// What an ephemeral token is: Google mints a short-lived, single-use resource
// name (`authTokens/…`) that the CLIENT passes to the Live API WebSocket in
// place of an API key. Media therefore flows browser/app <-> Google directly;
// this backend never proxies audio and never hands out the real key.
//
// TWO NON-OBVIOUS FACTS, both established by live calls against Google during
// the #654 spike. Do not "simplify" either one away:
//
//  1. The RAW REST endpoint REJECTS `liveConnectConstraints`
//     (`400 Unknown name "liveConnectConstraints" at 'auth_token'`) even
//     though the public docs show that field. The wire shape it actually
//     wants is `bidiGenerateContentSetup` + `fieldMask`. `@google/genai`'s
//     `authTokens.create()` performs exactly that transform, which is why
//     this module goes through the SDK instead of a hand-rolled fetch.
//  2. The response body is just `{ "name": "authTokens/…" }`. That `name` IS
//     the token; it is passed verbatim by the client. There is no separate
//     `token` field to read.
//
// `lockAdditionalFields: []` is load-bearing: it locks ONLY the fields we set
// in the constraint (model + response modality) and leaves the rest of
// LiveConnectConfig — system instruction, transcription — client-settable.
// Omitting it makes the SDK drop the field mask entirely, which changes the
// locking semantics.

import type { GoogleGenAI, LiveConnectConfig, Modality } from '@google/genai';

// Preview Live models get renamed/retired on short notice, so the id is env-
// driven and echoed back in the response: rotating it is `vercel env` + a
// redeploy, never a client release (#655/#656).
export const DEFAULT_GEMINI_LIVE_MODEL = 'models/gemini-3.1-flash-live-preview';

// One use, one session, one short window. These are the only server-side cost
// bounds that survive a modified client, so keep them tight.
export const AI_TOKEN_USES = 1;
export const AI_TOKEN_NEW_SESSION_WINDOW_MS = 30_000; // time to OPEN the session
export const AI_TOKEN_LIFETIME_MS = 12 * 60_000; // hard cap on the session itself
export const AI_TOKEN_RESPONSE_MODALITY = 'AUDIO';

export class GeminiConfigError extends Error {
  constructor(message = 'Gemini not configured') {
    super(message);
    this.name = 'GeminiConfigError';
  }
}

export interface GeminiEnv {
  apiKey: string;
  model: string;
  // Escape hatch for the Live-token API moving between API versions. Unset
  // means the SDK default (v1beta), which is what the #654 spike verified.
  apiVersion?: string;
}

// Missing GEMINI_API_KEY is the documented GLOBAL KILL SWITCH for hosted AI
// chat: unset it in Vercel and every /api/ai-token call returns 503 without
// touching Google. Clients render that as "AI chat temporarily unavailable"
// (#656), not a generic failure.
export function loadGeminiEnv(): GeminiEnv {
  const apiKey = process.env.GEMINI_API_KEY?.trim();
  if (!apiKey) {
    throw new GeminiConfigError('Gemini not configured: set GEMINI_API_KEY');
  }
  const model = process.env.GEMINI_LIVE_MODEL?.trim() || DEFAULT_GEMINI_LIVE_MODEL;
  const apiVersion = process.env.GEMINI_API_VERSION?.trim() || undefined;
  return { apiKey, model, apiVersion };
}

export interface EphemeralTokenRequest {
  uses: number;
  expireTime: string; // RFC3339
  newSessionExpireTime: string; // RFC3339
  responseModality: string;
}

export interface EphemeralToken {
  token: string; // the `authTokens/…` resource name, passed verbatim by clients
  // RFC3339, exactly as Google reported it on the CREATED token — never the
  // value we asked for. ABSENT when the create response carried none: Google is
  // documented to return `{ "name": … }` alone, and echoing our own request
  // back would present a modelled number as a measured one. Callers must read
  // absence as "unknown", not as "the request was honoured".
  expireTime?: string;
  model: string;
}

export type GeminiTokenMinter = (
  env: GeminiEnv,
  request: EphemeralTokenRequest
) => Promise<EphemeralToken>;

// Loaded lazily so the SDK is not pulled into the cold start of /api/token and
// /api/rooms, which import lib/handlers.ts but never mint a Gemini token.
let genaiModule: Promise<typeof import('@google/genai')> | undefined;
function loadGenai(): Promise<typeof import('@google/genai')> {
  genaiModule ??= import('@google/genai');
  return genaiModule;
}

let cachedClient: { apiKey: string; apiVersion?: string; client: GoogleGenAI } | undefined;

async function geminiClient(env: GeminiEnv): Promise<GoogleGenAI> {
  if (cachedClient && cachedClient.apiKey === env.apiKey && cachedClient.apiVersion === env.apiVersion) {
    return cachedClient.client;
  }
  const { GoogleGenAI: Client } = await loadGenai();
  const client = new Client({
    apiKey: env.apiKey,
    ...(env.apiVersion ? { httpOptions: { apiVersion: env.apiVersion } } : {}),
  });
  cachedClient = { apiKey: env.apiKey, apiVersion: env.apiVersion, client };
  return client;
}

// Google's own expiry for the created token, when it reports one. The SDK's
// `AuthToken` type declares only `name`, but `authTokens.create` returns the
// parsed response body unchanged, so any additional field Google sends really
// is there at runtime — it just has to be read defensively.
function reportedExpireTime(created: unknown): string | undefined {
  const value = (created as { expireTime?: unknown } | null | undefined)?.expireTime;
  if (typeof value !== 'string') return undefined;
  const trimmed = value.trim();
  return trimmed || undefined;
}

export const mintGeminiEphemeralToken: GeminiTokenMinter = async (env, request) => {
  const client = await geminiClient(env);
  const config: LiveConnectConfig = {
    // `Modality` is a string enum whose AUDIO member is literally "AUDIO".
    // Casting the plain string keeps this module's SDK import type-only, so
    // nothing loads @google/genai until a token is actually minted.
    responseModalities: [request.responseModality as Modality],
  };
  const created = await client.authTokens.create({
    config: {
      uses: request.uses,
      expireTime: request.expireTime,
      newSessionExpireTime: request.newSessionExpireTime,
      liveConnectConstraints: { model: env.model, config },
      lockAdditionalFields: [],
    },
  });
  const name = created?.name?.trim();
  if (!name) {
    // Never echo the response body — it is the only place a token-shaped
    // value could be logged from.
    throw new Error('gemini auth token response carried no token name');
  }
  // Report Google's expiry, or none at all. Substituting `request.expireTime`
  // here (as this did until #655's cost review) reported a value we had merely
  // ASKED for as the one Google granted — indistinguishable to every caller
  // from a measurement, and wrong the moment Google clamps or ignores it.
  const expireTime = reportedExpireTime(created);
  return { token: name, ...(expireTime ? { expireTime } : {}), model: env.model };
};
