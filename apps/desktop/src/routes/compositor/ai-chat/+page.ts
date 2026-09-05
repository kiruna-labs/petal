// Dedicated webview route for a remote compositor window's receiver-side
// AI-chat transcript/typed-message overlay (#844). Prerendered to
// build/compositor/ai-chat.html for the native compositor to host as a child
// webview (see src-tauri/src/compositor.rs's `create_ai_chat_overlay`).
export const ssr = false;
export const prerender = true;
