// Tauri doesn't have a Node.js server to do proper SSR
// so we use adapter-static with a fallback to index.html to put the site in SPA mode
// See: https://svelte.dev/docs/kit/single-page-apps
// See: https://v2.tauri.app/start/frontend/sveltekit/ for more info
import adapter from "@sveltejs/adapter-static";
import { vitePreprocess } from "@sveltejs/vite-plugin-svelte";
import { cpSync, mkdirSync, rmSync } from "node:fs";
import { join, relative, sep } from "node:path";

const sourceRoot = "src";
const sourceRoutes = join(sourceRoot, "routes");
const releaseRoutes =
  process.env.npm_lifecycle_event === "build" &&
  process.env.PETAL_INCLUDE_DEV_ROUTES !== "1"
    ? prepareReleaseRoutes()
    : sourceRoutes;

function prepareReleaseRoutes() {
  const targetRoot = ".svelte-kit/src-release";
  const targetRoutes = join(targetRoot, "routes");

  rmSync(targetRoot, { recursive: true, force: true });
  mkdirSync(targetRoot, { recursive: true });

  cpSync(sourceRoot, targetRoot, {
    recursive: true,
    filter: (source) => {
      const sourceRelative = relative(sourceRoot, source);
      return (
        sourceRelative !== join("routes", "dev") &&
        !sourceRelative.startsWith(`routes${sep}dev${sep}`)
      );
    },
  });

  return targetRoutes;
}

/** @type {import('@sveltejs/kit').Config} */
const config = {
  preprocess: vitePreprocess(),
  kit: {
    files: {
      routes: releaseRoutes,
    },
    adapter: adapter({
      fallback: "index.html",
    }),
    // @petal/shared -> repo-root shared/ package (design tokens, shared
    // components, shared logic). SvelteKit generates the tsconfig paths AND
    // the Vite alias from here — one location consumed by BOTH this app and
    // web-harness; never duplicate files into per-client copies. Value is
    // relative to the project root (apps/desktop): ../../shared = repo-root
    // shared/.
    alias: {
      "@petal/shared": "../../shared",
    },
  },
};

export default config;
