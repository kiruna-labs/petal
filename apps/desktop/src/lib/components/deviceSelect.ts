export function nextDeviceOptionIndex(
  current: number,
  key: string,
  count: number
): number | null {
  if (count === 0 || key === 'Escape') return null;
  if (key === 'Home') return 0;
  if (key === 'End') return count - 1;
  if (key === 'ArrowDown') return (current + 1) % count;
  if (key === 'ArrowUp') return (current - 1 + count) % count;
  return current;
}
