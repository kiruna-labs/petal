import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const styleSource = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');
const inviteToastSource = readFileSync(new URL('../src/inviteToast.ts', import.meta.url), 'utf8');
const controlsSource = readFileSync(new URL('../src/controls.ts', import.meta.url), 'utf8');
const homeScreenSource = readFileSync(new URL('../src/homeScreen.ts', import.meta.url), 'utf8');

function cssBlock(source: string, selector: string): string {
  const marker = `${selector} {`;
  const start = source.indexOf(marker);
  assert.notEqual(start, -1, `missing CSS block for ${selector}`);
  const bodyStart = start + marker.length;
  const end = source.indexOf('}', bodyStart);
  assert.notEqual(end, -1, `unterminated CSS block for ${selector}`);
  return source.slice(bodyStart, end);
}

test('web invite copied toast uses a two-line message and wraps instead of truncating', () => {
  const toastStyles = cssBlock(styleSource, '.toast');

  assert.match(inviteToastSource, /INVITE_LINK_COPIED_LABEL = 'Invite link copied to clipboard:';/);
  assert.match(inviteToastSource, /`\$\{INVITE_LINK_COPIED_LABEL\}\\n\$\{url\}`/);
  assert.match(controlsSource, /showToast\(inviteLinkCopiedToastMessage\(url\)\)/);
  assert.match(homeScreenSource, /showToast\?\.\(inviteLinkCopiedToastMessage\(url\)\)/);
  assert.match(toastStyles, /max-width:\s*min\(360px, calc\(100vw - 48px\)\);/);
  assert.match(toastStyles, /overflow-wrap:\s*anywhere;/);
  assert.match(toastStyles, /white-space:\s*pre-line;/);
  assert.doesNotMatch(toastStyles, /overflow:\s*hidden;/);
  assert.doesNotMatch(toastStyles, /text-overflow:\s*ellipsis;/);
  assert.doesNotMatch(toastStyles, /white-space:\s*nowrap;/);
});
