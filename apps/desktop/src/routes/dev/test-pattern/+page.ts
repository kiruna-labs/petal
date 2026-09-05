// Dev-only deterministic test-pattern window (#256), opened natively by the
// test cockpit for SHARE-N2W-Q. Prerender so adapter-static emits a real
// build/dev/test-pattern.html -- without this the native WebviewUrl
// ("dev/test-pattern.html") 404s to the SPA fallback, the actual test-pattern
// component never mounts, and the shared window shows frozen/static content
// (the SHARE-N2W-Q <1fps failure).
export const ssr = false;
export const prerender = true;
