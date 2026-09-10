// Re-export of the shared participant-name formatter (#122) -- see
// shared/logic/participantNames.ts for the single source of truth, which the
// web client's tile labels also consume. Mirrors the existing localEcho.ts /
// joinInput.ts / meetingCode.ts / strokeExpiry.ts re-export pattern here.
export * from '@petal/shared/logic/participantNames';
