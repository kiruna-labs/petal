// The first-party plugins compiled into BOTH clients. Sources arrive as raw
// text via RELATIVE `?raw` imports from this file's real location
// (shared/plugin-host -> ../../plugins). That resolves identically in the
// desktop app, the web dev server, Vercel's staged deploy (where
// scripts/deploy-web-harness.sh dereferences the web-harness/plugins symlink
// next to the shared/ copy), and every rendered test that aliases
// @petal/shared -- with no second alias to keep in sync. Built-ins are
// buildless single files precisely so this import needs no prior build step.
//
// This module is Vite-only (`?raw`); never import it from node:test files.
// Everything testable lives in settingsModel.ts / manifest.ts.

import reactionsManifestText from '../../plugins/reactions/manifest.json?raw';
import reactionsSource from '../../plugins/reactions/plugin.js?raw';
import { validateManifest } from './manifest.ts';
import type { InstalledPlugin } from './settingsModel.ts';

interface BuiltinSpec {
  manifestText: string;
  source: string;
  enabledByDefault: boolean;
}

const SPECS: BuiltinSpec[] = [{ manifestText: reactionsManifestText, source: reactionsSource, enabledByDefault: true }];

/** Validated built-ins. A built-in that fails validation is a build bug; it is skipped and reported. */
export function builtinPlugins(warn: (message: string) => void = () => {}): InstalledPlugin[] {
  const out: InstalledPlugin[] = [];
  for (const spec of SPECS) {
    let parsed: unknown;
    try {
      parsed = JSON.parse(spec.manifestText);
    } catch (e) {
      warn(`built-in plugin manifest is not JSON: ${String(e)}`);
      continue;
    }
    const result = validateManifest(parsed);
    if (!result.ok) {
      warn(`built-in plugin manifest invalid: ${result.errors.join('; ')}`);
      continue;
    }
    out.push({ manifest: result.manifest, source: 'builtin', enabledByDefault: spec.enabledByDefault, source_js: spec.source });
  }
  return out;
}
