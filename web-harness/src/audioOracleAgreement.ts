/**
 * AUD-N2W's recorded-waveform vs inbound-rtp stats agreement (#822).
 *
 * The two oracles must describe the same window. If they straddle the
 * audibility bar in opposite directions, that is an instrument failure, not
 * a product silence/audible verdict.
 *
 * `recordedRms < 0` means the recording path was unavailable; that path
 * already falls back to stats and must not throw here.
 */

export const AUDIBILITY_RMS_BAR = 0.01;

export function assertRemoteAudioOraclesAgree(
  recordedRms: number,
  statsRms: number,
  bar = AUDIBILITY_RMS_BAR
): void {
  if (recordedRms < 0) return;
  const recordedAudible = recordedRms >= bar;
  const statsAudible = statsRms >= bar;
  if (recordedAudible !== statsAudible) {
    throw new Error(
      `recorded waveform rms=${recordedRms.toFixed(4)} and inbound-rtp stats rms=${statsRms.toFixed(4)} disagree across the ${bar} audibility bar -- cannot measure audibility`
    );
  }
}
