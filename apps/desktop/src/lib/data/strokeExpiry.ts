// Re-export of the shared draw-stroke expiry predicate (#670) -- see
// shared/logic/strokeExpiry.ts for the single source of truth. Mirrors the
// existing localEcho.ts / joinInput.ts / meetingCode.ts re-export pattern in
// this directory.
export * from '@petal/shared/logic/strokeExpiry';
