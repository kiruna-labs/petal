// #377: the web client's usage-statistics opt-out, in the profile card on the
// home screen so it can be turned off before the first meeting. Copy and the
// stored choice come from shared/logic/usageStats.ts (same text as desktop).
import {
  USAGE_STATS_DESCRIPTION,
  USAGE_STATS_LABEL,
  setUsageStatsEnabled,
  usageStatsEnabled,
  type UsageStatsStorage,
} from '@petal/shared/logic/usageStats';

export function installUsageStatsToggle(
  doc: Document,
  storage: UsageStatsStorage | null = typeof localStorage === 'undefined' ? null : localStorage
): HTMLInputElement | null {
  const input = doc.querySelector<HTMLInputElement>('#usage-stats-checkbox');
  if (!input) return null;
  const title = doc.querySelector<HTMLElement>('#usage-stats-title');
  const description = doc.querySelector<HTMLElement>('#usage-stats-description');
  if (title) title.textContent = USAGE_STATS_LABEL;
  if (description) description.textContent = USAGE_STATS_DESCRIPTION;
  input.checked = usageStatsEnabled(storage);
  input.addEventListener('change', () => setUsageStatsEnabled(input.checked, storage));
  return input;
}
