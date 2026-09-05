// Shared Vite configuration for building a plugin to ONE self-contained ESM
// file, `dist/plugin.js`. Plugins import it from their own vite.config.ts:
//
//   import { pluginLibConfig } from '@petal/plugin-sdk/vite';
//   export default pluginLibConfig(import.meta.dirname);
//
// Constraints the config enforces: no externals (the frame cannot load
// anything), ES2022 target, no code splitting, no hashed filenames, and the
// `?raw` import path the built-ins use is the plain `dist/plugin.js`.

import type { UserConfig } from 'vite';
import { resolve } from 'node:path';

export function pluginLibConfig(root: string, entry = 'src/index.ts'): UserConfig {
  return {
    root,
    logLevel: 'warn',
    build: {
      target: 'es2022',
      outDir: resolve(root, 'dist'),
      emptyOutDir: true,
      minify: false,
      sourcemap: false,
      cssCodeSplit: false,
      lib: {
        entry: resolve(root, entry),
        formats: ['es'],
        fileName: () => 'plugin.js',
      },
      rollupOptions: {
        external: [],
        output: { inlineDynamicImports: true },
      },
    },
  };
}
