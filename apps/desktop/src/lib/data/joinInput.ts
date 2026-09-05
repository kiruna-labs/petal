// Path-stability shim: the implementation lives in the shared package
// (shared/logic/joinInput.ts) — the SINGLE SOURCE OF TRUTH consumed by both
// this app and web-harness. The desktop client's historical contract returns
// the short access code (`parseJoinInput` -> `{ ok, room }`), so this shim
// maps the shared module's desktop variant under the original name. Do not
// add logic here; edit the shared module.

export type JoinInputResult = { ok: true; room: string } | { ok: false; error: string };

export { normalizeJoinRoomName, parseJoinInputAccessCode as parseJoinInput } from '@petal/shared/logic/joinInput';
