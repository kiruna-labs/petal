// The first-party plugins compiled into BOTH clients. Each is the vendored
// registry artifact `plugins/builtins/<id>/bundle.json` (built from a pinned
// commit of kiruna-labs/petal-plugins; see plugins/builtins/README.md and
// SOURCES.json), imported as raw text via a RELATIVE `?raw` import from this
// file's real location (shared/plugin-host -> ../../plugins). That resolves
// identically in the desktop app, the web dev server, Vercel's staged deploy
// (where scripts/deploy-web-harness.sh dereferences the web-harness/plugins
// symlink next to the shared/ copy), and every rendered test that aliases
// @petal/shared -- with no second alias to keep in sync, and no build step
// at app-build time because the bundle is already packed.
//
// This module is Vite-only (`?raw`); never import it from node:test files.
// Everything testable lives in bundle.ts / settingsModel.ts / manifest.ts.

import reactionsBundleText from '../../plugins/builtins/petal.reactions/bundle.json?raw';
import { parseBundle } from './bundle.ts';
import type { InstalledPlugin } from './settingsModel.ts';

interface BuiltinSpec {
  bundleText: string;
  enabledByDefault: boolean;
}

const SPECS: BuiltinSpec[] = [{ bundleText: reactionsBundleText, enabledByDefault: true }];

/** Validated built-ins. A built-in that fails validation is a build bug; it is skipped and reported. */
export function builtinPlugins(warn: (message: string) => void = () => {}): InstalledPlugin[] {
  const out: InstalledPlugin[] = [];
  for (const spec of SPECS) {
    const parsed = parseBundle(spec.bundleText);
    if (!parsed.ok) {
      warn(`built-in plugin bundle unusable: ${parsed.error}`);
      continue;
    }
    out.push({ manifest: parsed.bundle.manifest, source: 'builtin', enabledByDefault: spec.enabledByDefault, source_js: parsed.bundle.source });
  }
  return out;
}
