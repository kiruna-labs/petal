// Counting how many people take a build — and recording nothing else about
// them.
//
// WHY THIS EXISTS (#125). `/api/download` used to record nothing at all: it
// validated `?platform=` and 302'd. When "how many people took the Windows
// build before 0.9.15, while every Windows install was permanently stranded?"
// finally had to be answered, the only source was the Vercel request log —
// dashboard-gated, short retention, carrying no resolved version, and equally
// unavailable the next time the question is asked. This module does not
// recover that window; it makes the question answerable from here on.
//
// PRIVACY POLICY (allowlist-first, same posture as lib/sentry.ts, and NOT
// negotiable). Exactly three values ever leave this process — `event`,
// `platform`, `version` — and every one of them is constrained here to a
// closed set or a strict version shape before it is emitted. Nothing is ever
// read off the request: no IP, no User-Agent, no referrer, no header, no
// query string, no identity, no session. That is enforced structurally, not
// by convention: these functions take primitives, never a `VercelRequest`, so
// a future call site physically cannot hand one in. The output is a count,
// never a profile — this is a public marketing endpoint.
//
// WHY A CONSOLE LINE AND NOT AN ANALYTICS SDK.
//   - PostHog is ruled out by docs/POSTHOG_EVENT_ALLOWLIST.md, verbatim:
//     "Do not add events from the backend."
//   - Sentry is the crash tool. lib/sentry.ts is allowlist-first and
//     error-only, `captureApiError` is documented as the ONLY call site that
//     may talk to `captureException`, and 4xx are deliberately not captured
//     as a quota guardrail — a per-download success event is exactly the
//     traffic that guardrail exists to keep out. It is also the wrong shape
//     for latency: every Sentry path must `await flushSentry(2000)` before
//     Vercel freezes the function, so counting through Sentry would put up to
//     two seconds in front of a redirect.
//   - So: one structured line on stdout. No dependency, no network call, no
//     added latency, picked up by Vercel's runtime logs and by any log drain
//     pointed at this project. It is the honest minimum. Note what it does
//     NOT do on its own: retention is still whatever the platform gives us,
//     so durable history needs a drain configured. What changes today is that
//     the number exists, is stably named, and carries the resolved version.
//
// The line is `petal.metric {"event":...}` — a fixed prefix so it can be
// grepped or matched by a drain without parsing every log line.

export const DISTRIBUTION_METRIC_PREFIX = 'petal.metric';

export type DistributionPlatform = 'macos' | 'windows';

// Emitted when the platform or version could not be resolved into its
// allowed shape. Never a passthrough of the rejected value.
export const UNKNOWN = 'unknown';

export interface DownloadMetric {
  event: 'download';
  platform: DistributionPlatform | typeof UNKNOWN;
  version: string;
}

// The only shape `version` may ever take: semver `major.minor.patch` with an
// optional prerelease/build suffix, anchored at both ends.
//
// Deliberately a GRAMMAR, not a charset. An earlier draft gated on
// `^[0-9A-Za-z][0-9A-Za-z.+-]{0,39}$` and the privacy test in
// test/privacy.ts caught what that lets through: `203.0.113.7` — an IPv4
// address — is digits and dots and passes a charset gate cleanly. Requiring
// exactly three numeric components rejects it. Do not loosen this back into
// a character class.
const RELEASE_VERSION = /^\d{1,5}\.\d{1,5}\.\d{1,5}(?:[-+][0-9A-Za-z.-]{1,32})?$/;

// Release artifacts are `Petal_<version>_universal.dmg` and
// `Petal_<version>_windows_x86_64-setup.exe` (see lib/blob.ts). Anchored and
// closed: a pathname that does not match yields `unknown` rather than any
// part of itself.
const ARTIFACT_PATHNAME = /^Petal_(\d{1,5}\.\d{1,5}\.\d{1,5}(?:[-+][0-9A-Za-z.-]{1,32})?)_/;

export function versionFromArtifactPathname(pathname: unknown): string {
  if (typeof pathname !== 'string') return UNKNOWN;
  const match = ARTIFACT_PATHNAME.exec(pathname);
  if (!match) return UNKNOWN;
  return RELEASE_VERSION.test(match[1]) ? match[1] : UNKNOWN;
}

export function safePlatform(platform: unknown): DistributionPlatform | typeof UNKNOWN {
  return platform === 'macos' || platform === 'windows' ? platform : UNKNOWN;
}

// The exact bytes that reach the sink. Exported so tests can assert the
// payload without spying on console, and so the field set is pinned in one
// place: this object literal is the allowlist.
export function distributionMetricLine(metric: DownloadMetric): string {
  return `${DISTRIBUTION_METRIC_PREFIX} ${JSON.stringify({
    event: metric.event,
    platform: safePlatform(metric.platform),
    version: RELEASE_VERSION.test(metric.version) ? metric.version : UNKNOWN,
  })}`;
}

// Count one served download. Call AFTER the redirect has been written.
//
// Never throws, by construction — a broken or replaced log transport must not
// turn a working download into a failed one. A missing count is a missing
// count; a failed redirect is a person who cannot install Petal.
export function recordDownload(platform: unknown, artifactPathname: unknown): void {
  try {
    console.log(
      distributionMetricLine({
        event: 'download',
        platform: safePlatform(platform),
        version: versionFromArtifactPathname(artifactPathname),
      })
    );
  } catch {
    // Intentionally swallowed — see above. Do not add a fallback `console.*`
    // here: the thing that just failed IS console.
  }
}
