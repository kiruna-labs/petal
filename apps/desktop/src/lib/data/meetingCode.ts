// Path-stability shim: the implementation lives in the shared package
// (shared/logic/meetingCode.ts) — the SINGLE SOURCE OF TRUTH consumed by both
// this app and web-harness. Do not add logic here; edit the shared module.
export * from '@petal/shared/logic/meetingCode';
