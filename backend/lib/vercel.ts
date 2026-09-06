// Local stand-in for the two types this codebase imported from `@vercel/node`.
//
// `@vercel/node` was a runtime dependency used ONLY for these type imports:
// the real request/response objects are supplied by the Vercel Node runtime
// at deploy time, never by this package. Keeping it pulled in a large,
// stale transitive tree (tar 6, undici 5, esbuild 0.14 -- 40 of the 46 open
// Dependabot alerts, llitllit/petal#928) that even its latest major still
// pins. These definitions mirror @vercel/node's public `VercelRequest` /
// `VercelResponse` exactly, so handlers and tests type-check unchanged.
import type { IncomingMessage, ServerResponse } from "node:http";

export type VercelRequestCookies = { [key: string]: string };
export type VercelRequestQuery = { [key: string]: string | string[] };
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type VercelRequestBody = any;

export type VercelRequest = IncomingMessage & {
  query: VercelRequestQuery;
  cookies: VercelRequestCookies;
  body: VercelRequestBody;
};

export type VercelResponse = ServerResponse & {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  send: (body: any) => VercelResponse;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  json: (jsonBody: any) => VercelResponse;
  status: (statusCode: number) => VercelResponse;
  redirect: (statusOrUrl: string | number, url?: string) => VercelResponse;
};
