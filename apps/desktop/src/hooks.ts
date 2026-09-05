// SvelteKit universal reroute hook — the fix for the "404 panels scattered
// across the desktop" bug.
//
// Every borderless native panel (hover-tab, share-border, menubar-popover,
// the compositor windows, dev/telepointer) is created on the Rust side with
// a URL like `WebviewUrl::App("hover-tab.html")`, so the webview loads
// `tauri://localhost/hover-tab.html`. The prerendered file exists and is
// correct, but because `ssr = false` the page boots "cold" and the SvelteKit
// client router must resolve the route from `location.pathname` —
// `/hover-tab.html`. The compiled route table has keys WITHOUT the `.html`
// suffix (`/hover-tab`, `/share-border`, `/compositor/surface`, …), each
// anchored as `^/hover-tab/?$`, which does NOT match `/hover-tab.html`. No
// route matches → SvelteKit renders its built-in error page, which literally
// shows "404". The hover-tab panel is the one the user sees "scattered/moving"
// because it's created at startup and follows the cursor.
//
// `reroute` runs before route matching on both initial load and client
// navigation. Stripping the `.html` (and any `/index.html`) makes the router
// resolve `/hover-tab.html` → `/hover-tab`, matching the existing route table.
// It does NOT change the requested file or the URL bar — only which route the
// client router resolves. One hook fixes every panel with zero Rust changes.
import type { Reroute } from '@sveltejs/kit';

export const reroute: Reroute = ({ url }) => {
  const p = url.pathname;
  if (p.endsWith('/index.html')) {
    return p.slice(0, -'/index.html'.length) || '/';
  }
  if (p.endsWith('.html')) {
    return p.slice(0, -'.html'.length);
  }
  return p;
};
