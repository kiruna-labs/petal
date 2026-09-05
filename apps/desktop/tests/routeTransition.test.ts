import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const layoutSource = readFileSync(new URL('../src/routes/+layout.svelte', import.meta.url), 'utf8');
const mainMeetingActionsSource = readFileSync(
  new URL('../src/lib/data/mainMeetingActions.ts', import.meta.url),
  'utf8'
);
const meetingSessionSource = readFileSync(
  new URL('../src/lib/meeting/meetingSession.svelte.ts', import.meta.url),
  'utf8'
);
const pillWindowSource = readFileSync(
  new URL('../src/lib/meeting/pillWindow.svelte.ts', import.meta.url),
  'utf8'
);

test('layout registers the view-transition hook for client-side navigation', () => {
  assert.match(
    layoutSource,
    /import\s*\{[^}]*\bonNavigate\b[^}]*\}\s*from\s*'\$app\/navigation'/
  );
  assert.match(layoutSource, /document\.startViewTransition/);
  assert.match(layoutSource, /await navigation\.complete/);
  assert.match(layoutSource, /view-transition-name:\s*petal-route/);
  assert.match(layoutSource, /prefers-reduced-motion: reduce/);
});

test('main menu pre-sizes the meeting window before navigating', () => {
  assert.match(mainMeetingActionsSource, /prepareMeetingWindow/);
  assert.match(mainMeetingActionsSource, /await\s+prepareMeetingWindow\(\)/);
});

test('meeting session restores home geometry before returning to /main', () => {
  assert.match(meetingSessionSource, /prepareReturnToHome/);
});

test('pillWindow exports the shared meeting-geometry entry helper', () => {
  assert.match(pillWindowSource, /export async function prepareMeetingWindow/);
});
