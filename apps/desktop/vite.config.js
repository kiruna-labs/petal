// Tauri doesn't have a Node.js server to do proper SSR
// so we use adapter-static with a fallback to index.html to put the site in SPA mode
// See: https://svelte.dev/docs/kit/single-page-apps
// See: https://v2.tauri.app/start/frontend/sveltekit/ for more info
import adapter from "@sveltejs/adapter-static";

import { vitePreprocess } from "@sveltejs/vite-plugin-svelte";
import { cpSync, mkdirSync, rmSync } from "node:fs";
import { join, relative, sep } from "node:path";
import { defineConfig } from "vite";
import { sveltekit } from "@sveltejs/kit/vite";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;

const sourceRoot = "src";
const sourceRoutes = join(sourceRoot, "routes");
const releaseRoutes = process.env.npm_lifecycle_event === "build" && process.env.PETAL_INCLUDE_DEV_ROUTES !== "1" ? prepareReleaseRoutes() : sourceRoutes;

function prepareReleaseRoutes() {
  const targetRoot = ".svelte-kit/src-release";
  const targetRoutes = join(targetRoot, "routes");

  rmSync(targetRoot, { recursive: true, force: true });
  mkdirSync(targetRoot, { recursive: true });

  cpSync(sourceRoot, targetRoot, {
    recursive: true,
    filter: (source) => {
      const sourceRelative = relative(sourceRoot, source);

      return sourceRelative !== join("routes", "dev") && !sourceRelative.startsWith(`routes${sep}dev${sep}`);
    }
  });

  return targetRoutes;
}

// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [
    sveltekit({
      preprocess: vitePreprocess(),
      files: { routes: releaseRoutes },
      adapter: adapter({ fallback: "index.html" }),
      // Kit 3 polls _app/version.json hourly by default; the Tauri bundle
      // ships its own assets and never reads `updated`, so keep it off.
      version: { pollInterval: 0 },
      // @petal/shared -> repo-root shared/ package (design tokens, shared
      // components, shared logic). SvelteKit generates the tsconfig paths AND
      // the Vite alias from here — one location consumed by BOTH this app and
      // web-harness; never duplicate files into per-client copies. Value is
      // relative to the project root (apps/desktop): ../../shared = repo-root
      // shared/.
      alias: { "@petal/shared": "../../shared" }
    })
  ],

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    // The shared/ package lives outside this app's root — allow the dev
    // server to serve it (tokens/components/logic imported via
    // `@petal/shared`).
    fs: {
      allow: ["../.."],
    },
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
