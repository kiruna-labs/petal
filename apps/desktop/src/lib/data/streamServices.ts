// Ready-made server URLs for the go-live dialog's "Service" picker. The URL
// is the same for every account on a service; only the stream key is
// personal, so a preset fills the URL (still editable) and the user pastes
// their key.
//
// Ingest addresses change now and then. Re-verify before each release with
// `node apps/desktop/scripts/check-stream-presets.mjs` (docs/RELEASING.md),
// and bump STREAM_SERVICES_VERIFIED_ON when the list is confirmed or fixed.
// src-tauri/src/streaming/rtmp.rs parses every URL below in a unit test.

export const STREAM_SERVICES_VERIFIED_ON = '2026-10-11';

export interface StreamService {
  id: string;
  name: string;
  /** Empty for Custom. */
  serverUrl: string;
  /** Where the user finds their stream key (and the URL, for Custom-ish services). */
  helpUrl?: string;
  /** Shown under the URL field when the preset may need adjusting. */
  hint?: string;
  /** Shown in the go-live dialog for this service, new or saved. */
  goLiveHint?: string;
  /** Every ingest host under this domain is this service, not only the
   *  preset's (YouTube hands out RTMP, RTMPS and backup addresses). */
  ingestDomain?: string;
}

export const CUSTOM_STREAM_SERVICE_ID = 'custom';
const CUSTOM_STREAM_SERVICE: StreamService = {
  id: CUSTOM_STREAM_SERVICE_ID,
  name: 'Custom',
  serverUrl: ''
};

export const STREAM_SERVICES: readonly StreamService[] = [
  {
    // YouTube Help "Encrypt your stream using RTMPS" (answer 10364924).
    id: 'youtube',
    name: 'YouTube',
    serverUrl: 'rtmps://a.rtmps.youtube.com:443/live2',
    helpUrl: 'https://support.google.com/youtube/answer/2907883',
    ingestDomain: 'youtube.com',
    // Until "Stream finished" is dismissed, YouTube accepts a new stream on
    // the key but has no broadcast to put it in, and waits forever.
    goLiveHint:
      'After a stream ends, YouTube Studio shows “Stream finished”. Choose Dismiss there before going live again, or YouTube won’t pick up the new stream.'
  },
  {
    // Twitch's ingest API (https://ingest.twitch.tv/ingests) "Default" entry,
    // which routes to the nearest ingest on its own.
    id: 'twitch',
    name: 'Twitch',
    serverUrl: 'rtmps://ingest.global-contribute.live-video.net/app',
    helpUrl: 'https://help.twitch.tv/s/article/twitch-stream-key-faq'
  },
  {
    id: 'facebook',
    name: 'Facebook Live',
    serverUrl: 'rtmps://live-api-s.facebook.com:443/rtmp/',
    helpUrl: 'https://www.facebook.com/business/help/995454917538649'
  },
  {
    // Kick Help Center's published ingest; the dashboard is authoritative.
    id: 'kick',
    name: 'Kick',
    serverUrl: 'rtmps://fa723fc1b171.global-contribute.live-video.net:443/app',
    helpUrl: 'https://help.kick.com/en/articles/7135578-how-to-connect-your-kick-stream-to-restream-service',
    hint: 'If your Kick dashboard shows a different Stream URL, use that one.'
  },
  CUSTOM_STREAM_SERVICE
];

export function streamServiceById(id: string): StreamService {
  return STREAM_SERVICES.find((service) => service.id === id) ?? CUSTOM_STREAM_SERVICE;
}

/** The preset whose URL this is, or whose ingest domain it's on, for
 *  labelling a typed, pasted or saved URL. */
export function streamServiceForUrl(serverUrl: string): StreamService | undefined {
  const normalized = serverUrl.trim().replace(/\/+$/, '');
  const exact = STREAM_SERVICES.find(
    (service) => service.serverUrl !== '' && service.serverUrl.replace(/\/+$/, '') === normalized
  );
  if (exact) return exact;
  let host: string;
  try {
    host = new URL(normalized).hostname.toLowerCase();
  } catch {
    return undefined;
  }
  return STREAM_SERVICES.find(
    ({ ingestDomain }) =>
      ingestDomain !== undefined && (host === ingestDomain || host.endsWith(`.${ingestDomain}`))
  );
}
