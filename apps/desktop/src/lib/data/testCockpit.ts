import type { CockpitJourney, CockpitStatus, CockpitSummary, TestProgressEvent } from '$lib/ipc';
import contracts from '../../../../../contracts/petal-contracts.json' with { type: 'json' };

interface CockpitScenarioDefinition {
  id: string;
  tier: string;
}

interface CockpitContracts {
  testCockpitScenarios: CockpitScenarioDefinition[];
  testCockpitJourneys: CockpitJourney[];
}

export interface CockpitTierOption {
  id: string;
  label: string;
}

export const COCKPIT_SCENARIOS = (contracts as CockpitContracts).testCockpitScenarios;

function cockpitTierLabel(tier: string): string {
  return tier
    .split(/[-_\s]+/)
    .filter(Boolean)
    .map((part) => `${part.slice(0, 1).toUpperCase()}${part.slice(1)}`)
    .join(' ');
}

export function availableCockpitTierOptions(
  scenarios: readonly CockpitScenarioDefinition[] = COCKPIT_SCENARIOS
): CockpitTierOption[] {
  const tiers = new Map<string, CockpitTierOption>();
  for (const scenario of scenarios) {
    const tier = scenario.tier.trim();
    if (!tier || tiers.has(tier)) continue;
    tiers.set(tier, { id: tier, label: cockpitTierLabel(tier) });
  }
  return [...tiers.values()];
}

// The legacy tier picker only surfaces the user-facing run presets. The opt-in
// scaffolding tiers introduced with the journey model (native, multi-display,
// gap, ui) gate hardware-specific / scaffold-only journeys and are launched from
// the feature/priority/journey UI (COCKPIT_PRESETS + per-journey ▶), never from
// this dropdown — so they're filtered out here.
const USER_FACING_TIERS = new Set(['quick', 'full', 'soak']);

export const COCKPIT_TIER_OPTIONS = availableCockpitTierOptions().filter((tier) =>
  USER_FACING_TIERS.has(tier.id)
);

export function cockpitSelectorFromInput(tier: string, scenarioIds: string): string {
  const ids = scenarioIds
    .split(',')
    .map((id) => id.trim())
    .filter(Boolean);
  return ids.length > 0 ? ids.join(',') : tier;
}

export function cockpitSummaryLine(summary: CockpitSummary | null | undefined): string | null {
  if (!summary) return null;
  const skipped = summary.skipped.length;
  const skippedText = skipped === 1 ? '1 skipped' : `${skipped} skipped`;
  return `${summary.status}: ${summary.passed} passed, ${summary.failed} failed, ${skippedText}`;
}

export function latestCockpitMessage(
  status: CockpitStatus | null,
  progress: TestProgressEvent[]
): string | null {
  const latest = progress.at(-1);
  if (latest?.message) return latest.message;
  return status?.summary?.message ?? null;
}

// ---------------------------------------------------------------------------
// Journey model (P-1) — feature-grouped view over the COCKPIT_TEST_MAP journeys.
// The journey metadata is authored in the project history and mirrored into
// contracts/petal-contracts.json → testCockpitJourneys (P-0). The Rust selector
// (start_test_cockpit) accepts a journey id, a feature code/slug, a priority, a
// depth, and the legacy tiers — so every builder below just returns a plain
// selector string the backend already understands.
// ---------------------------------------------------------------------------

export const COCKPIT_JOURNEYS: readonly CockpitJourney[] = (
  contracts as CockpitContracts
).testCockpitJourneys;

export interface CockpitFeature {
  code: string;
  name: string;
  slug: string;
}

/** Features A–H in document order, with the human names from the project history */
export const COCKPIT_FEATURES: readonly CockpitFeature[] = [
  { code: 'A', name: 'Screen Sharing', slug: 'screen-sharing' },
  { code: 'B', name: 'Camera', slug: 'camera' },
  { code: 'C', name: 'Audio', slug: 'audio' },
  { code: 'D', name: 'Remote Control', slug: 'remote-control' },
  { code: 'E', name: 'Telepointers & Annotation', slug: 'telepointers-annotation' },
  { code: 'F', name: 'Resilience', slug: 'resilience' },
  { code: 'G', name: 'Rooms & multi-peer', slug: 'rooms-multi-peer' },
  { code: 'H', name: 'UI correctness', slug: 'ui-correctness' }
];

export interface CockpitFeatureGroup extends CockpitFeature {
  journeys: CockpitJourney[];
  /** How many journeys in this feature can actually be run (have a runnable scenario). */
  runnableCount: number;
}

/** Journeys grouped under their feature, features kept in A–H order. */
export function cockpitFeatureGroups(
  journeys: readonly CockpitJourney[] = COCKPIT_JOURNEYS
): CockpitFeatureGroup[] {
  return COCKPIT_FEATURES.map((feature) => {
    const rows = journeys.filter((journey) => journey.feature === feature.code);
    return {
      ...feature,
      journeys: rows,
      runnableCount: rows.filter((journey) => journeyIsRunnable(journey)).length
    };
  });
}

/** A journey is runnable when it maps to a concrete scenario the backend can drive. */
export function journeyIsRunnable(journey: CockpitJourney): boolean {
  return Boolean(journey.runnable);
}

// --- Selector builders (all return a plain string the Rust selector accepts) ---

export function journeySelector(journey: CockpitJourney | string): string {
  return typeof journey === 'string' ? journey : journey.id;
}

export function featureSelector(feature: CockpitFeature | string): string {
  return typeof feature === 'string' ? feature : feature.code;
}

export function prioritySelector(priority: string): string {
  return priority.toLowerCase();
}

export function depthSelector(depth: string): string {
  return depth.toLowerCase();
}

export interface CockpitPreset {
  id: string;
  label: string;
  selector: string;
  description: string;
}

/** The three quick presets, mapped onto the legacy tier selectors the backend keeps. */
export const COCKPIT_PRESETS: readonly CockpitPreset[] = [
  { id: 'quick', label: 'P0 · Short', selector: 'quick', description: 'Critical short journeys' },
  { id: 'full', label: 'All short', selector: 'full', description: 'Every short journey' },
  { id: 'soak', label: 'Soak', selector: 'long', description: 'Long endurance journeys' }
];

// --- Display helpers ---------------------------------------------------------

const DIRECTION_LABELS: Record<string, string> = {
  'nat-nat': 'nat↔nat',
  'web-nat': 'web→nat',
  'nat-web': 'nat→web',
  both: 'nat↔web',
  'nat-local': 'nat-local'
};

export function directionLabel(direction: string): string {
  return DIRECTION_LABELS[direction] ?? direction;
}

export interface JourneyStatusInfo {
  marker: string;
  label: string;
  /** kebab token for CSS class hooks: covered | partial | gap | blind-spot */
  token: string;
}

const STATUS_INFO: Record<string, JourneyStatusInfo> = {
  covered: { marker: '✅', label: 'Covered', token: 'covered' },
  partial: { marker: '🟡', label: 'Partial', token: 'partial' },
  gap: { marker: '⛔', label: 'Gap', token: 'gap' },
  'blind-spot': { marker: '⚠️', label: 'Blind spot', token: 'blind-spot' }
};

export function journeyStatusInfo(status: string): JourneyStatusInfo {
  return STATUS_INFO[status] ?? { marker: '•', label: status, token: 'unknown' };
}

export function depthLabel(depth: string): string {
  switch (depth) {
    case 'short':
      return 'Short';
    case 'long':
      return 'Long';
    case 'short-long':
      return 'Short·Long';
    default:
      return depth;
  }
}

// --- Live run state ----------------------------------------------------------

export type JourneyLiveState = 'queued' | 'running' | 'passed' | 'failed' | 'skipped' | null;

/**
 * Map a raw `scenario-verdict` verdict (kebab-case from Rust) onto the coarse
 * pass/fail/skip states the row UI shows.
 */
export function verdictToLiveState(verdict: string): JourneyLiveState {
  switch (verdict.trim().toLowerCase()) {
    case 'pass':
      return 'passed';
    case 'test-fail':
    case 'infra-fail':
      return 'failed';
    case 'skipped':
    case 'cancelled':
      return 'skipped';
    default:
      return null;
  }
}

export const JOURNEY_LIVE_LABELS: Record<Exclude<JourneyLiveState, null>, string> = {
  queued: 'Queued',
  running: 'Running',
  passed: 'Pass',
  failed: 'Fail',
  skipped: 'Skipped'
};
