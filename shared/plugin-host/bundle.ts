// The registry/vendored bundle shape: `{ manifest, files: { [entry]: source } }`
// (packed by build-all.mjs in kiruna-labs/petal-plugins). One parser for the
// three places a bundle is read from: vendored built-ins (builtins.ts), the
// desktop's installed store (pluginCatalog.ts), and tests.
import { validateManifest, type PluginManifest } from './manifest.ts';

export interface ParsedBundle {
  manifest: PluginManifest;
  /** The entry file's source text. */
  source: string;
}

export type BundleParse = { ok: true; bundle: ParsedBundle } | { ok: false; error: string };

export function parseBundle(text: string): BundleParse {
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch (e) {
    return { ok: false, error: `bundle is not JSON: ${String(e)}` };
  }
  if (!raw || typeof raw !== 'object') return { ok: false, error: 'bundle is not an object' };
  const { manifest, files } = raw as { manifest?: unknown; files?: unknown };
  const validated = validateManifest(manifest);
  if (!validated.ok) return { ok: false, error: `bundle manifest invalid: ${validated.errors.join('; ')}` };
  const source = files && typeof files === 'object' ? (files as Record<string, unknown>)[validated.manifest.entry] : undefined;
  if (typeof source !== 'string' || source.length === 0) return { ok: false, error: `bundle lacks its entry file ${validated.manifest.entry}` };
  return { ok: true, bundle: { manifest: validated.manifest, source } };
}
