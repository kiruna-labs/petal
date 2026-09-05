import type { LocalVideoTrack } from 'livekit-client';
import type { LogKind } from './ui/logging';

interface CodecCheck {
  label: string;
  mimeType: string | null;
  ok: boolean;
}

interface CodecHarnessHook {
  codecChecks: CodecCheck[];
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// H.264 is load-bearing for the native compositor; record every negotiated
// codec check on window.__petalHarness.codecChecks for browser automation.
export async function verifyH264Negotiated(
  track: LocalVideoTrack,
  label: string,
  harnessHook: CodecHarnessHook,
  logEvent: (message: string, kind?: LogKind) => void
) {
  for (let attempt = 0; attempt < 10; attempt++) {
    await sleep(1000);
    const sender = track.sender;
    if (!sender) continue;
    let stats: RTCStatsReport;
    try {
      stats = await sender.getStats();
    } catch {
      continue;
    }
    let codecId: string | undefined;
    stats.forEach((s) => {
      if (s.type === 'outbound-rtp' && (s as RTCOutboundRtpStreamStats).codecId) {
        codecId = (s as RTCOutboundRtpStreamStats).codecId;
      }
    });
    if (!codecId) continue;
    let mimeType: string | null = null;
    stats.forEach((s) => {
      if (s.id === codecId) mimeType = (s as { mimeType?: string }).mimeType ?? null;
    });
    if (!mimeType) continue;
    const ok = /h264/i.test(mimeType);
    harnessHook.codecChecks.push({ label, mimeType, ok });
    if (ok) {
      logEvent(`${label}: negotiated codec ${mimeType} (native compositor can render this)`, 'ok');
    } else {
      logEvent(
        `${label}: NEGOTIATED CODEC IS ${mimeType}, NOT H.264 -- the native Petal app ` +
          `will NOT render this share (its compositor only renders VideoToolbox-decoded ` +
          `H.264; other codecs hit a deliberate no-render fallback). This browser likely ` +
          `refused H.264 encoding.`,
        'error'
      );
    }
    return;
  }
  harnessHook.codecChecks.push({ label, mimeType: null, ok: false });
  logEvent(`${label}: could not determine negotiated codec from RTP stats after 10s`, 'warn');
}
