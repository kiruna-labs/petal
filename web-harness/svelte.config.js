// Shared-component support for the web client: lets this Vite app compile and
// mount Svelte components from the shared package (shared/ui/components/*) —
// the same components the desktop app uses. vitePreprocess handles the
// components' `lang="ts"` scripts in both the build (vite plugin) and the
// check (svelte-check).
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/** @type {import('@sveltejs/vite-plugin-svelte').SvelteConfig} */
export default {
  preprocess: vitePreprocess()
};
