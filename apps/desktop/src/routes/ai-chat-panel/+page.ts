// Dedicated webview route for the floating AI chat panel (#738).
// No dynamic server data — prerender so adapter-static can emit a static
// build/ai-chat-panel/index.html for Tauri's WebviewUrl::App to point at.
export const ssr = false;
export const prerender = true;
