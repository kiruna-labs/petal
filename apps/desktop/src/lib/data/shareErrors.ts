export type ShareSessionError =
  | { kind: 'permissionDenied'; message?: unknown }
  | { kind: 'windowNotFound'; message?: unknown }
  | { kind: 'displayNotFound'; message?: unknown }
  | { kind: 'capture'; message?: unknown }
  | { kind: 'config'; message?: unknown }
  | { kind: 'roomConnect'; message?: unknown }
  | { kind: 'tooManyShares'; message?: unknown }
  | { kind: 'microphone'; message?: unknown }
  | { kind: 'notInRoom'; message?: unknown }
  | { kind: 'camera'; message?: unknown }
  | { kind: string; message?: unknown };

export interface ShareErrorDisplay {
  message: string;
  openScreenRecordingSettings: boolean;
}

function detail(value: unknown): string | null {
  if (typeof value === 'string' && value.trim()) return value.trim();
  if (typeof value === 'number' && Number.isFinite(value)) return String(value);
  return null;
}

function parseUnknownError(error: unknown): ShareSessionError {
  if (error && typeof error === 'object' && 'kind' in error) {
    return error as ShareSessionError;
  }
  if (typeof error === 'string') {
    return { kind: 'unknown', message: error };
  }
  return { kind: 'unknown' };
}

export function shareErrorDisplay(error: unknown): ShareErrorDisplay {
  const parsed = parseUnknownError(error);
  const message = detail(parsed.message);

  switch (parsed.kind) {
    case 'permissionDenied':
      return {
        message: 'Screen Recording is off - enable Petal in Privacy & Security, then relaunch',
        openScreenRecordingSettings: true
      };
    case 'tooManyShares':
      return {
        message: `You can share up to ${message ?? '4'} windows - stop one before sharing another`,
        openScreenRecordingSettings: false
      };
    case 'notInRoom':
      return {
        message: 'Join a room before sharing a window',
        openScreenRecordingSettings: false
      };
    case 'windowNotFound':
      return {
        message: 'That window is no longer available to share',
        openScreenRecordingSettings: false
      };
    case 'displayNotFound':
      return {
        message: 'That display is no longer available to share',
        openScreenRecordingSettings: false
      };
    case 'roomConnect':
      return {
        message: `Could not connect to the room${message ? ` - ${message}` : ''}`,
        openScreenRecordingSettings: false
      };
    case 'capture':
      if (message?.includes('Windows display-region capture could not maintain the GPU ROI path')) {
        return {
          message: 'Petal View GPU capture could not start safely',
          openScreenRecordingSettings: false
        };
      }
      return {
        message: `Could not capture that window${message ? ` - ${message}` : ''}`,
        openScreenRecordingSettings: false
      };
    case 'config':
      return {
        message: `Petal is not configured for meetings${message ? ` - ${message}` : ''}`,
        openScreenRecordingSettings: false
      };
    case 'microphone':
      return {
        message: `Microphone is unavailable${message ? ` - ${message}` : ''}`,
        openScreenRecordingSettings: false
      };
    case 'camera':
      return {
        message: `Camera is unavailable${message ? ` - ${message}` : ''}`,
        openScreenRecordingSettings: false
      };
    default:
      return {
        message: message ? `Could not share window - ${message}` : 'Could not share that window',
        openScreenRecordingSettings: false
      };
  }
}
