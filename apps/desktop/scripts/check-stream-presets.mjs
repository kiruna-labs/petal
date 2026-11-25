#!/usr/bin/env node
// Re-verify the go-live dialog's ready-made server URLs
// (src/lib/data/streamServices.ts). Run before each release (docs/RELEASING.md):
//
//   node apps/desktop/scripts/check-stream-presets.mjs
//
// For every preset: DNS resolves, a TLS handshake on its port succeeds with a
// certificate valid for the host. For Twitch, the URL must still be the one
// Twitch's own ingest API (https://ingest.twitch.tv/ingests) hands out. This
// proves the servers are there, not that a key would be accepted; when a
// platform moves its ingest, update the preset and STREAM_SERVICES_VERIFIED_ON.
// Needs network access; sends no stream keys.

import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { connect } from 'node:tls';
import { fileURLToPath } from 'node:url';

const TIMEOUT_MS = 10_000;
const here = dirname(fileURLToPath(import.meta.url));
const source = readFileSync(resolve(here, '../src/lib/data/streamServices.ts'), 'utf8');

const presets = [...source.matchAll(/id: '([a-z]+)',\s*name: '[^']*',\s*serverUrl: '([^']+)'/g)].map(
  ([, id, url]) => ({ id, url })
);
const verifiedOn = source.match(/STREAM_SERVICES_VERIFIED_ON = '([^']+)'/)?.[1];

function handshake(url) {
  return new Promise((resolvePromise, reject) => {
    let hostname, port;
    try {
      ({ hostname, port } = new URL(url.replace(/^rtmps:/, 'https:')));
    } catch {
      reject(new Error('not a valid URL'));
      return;
    }
    const socket = connect({ host: hostname, port: Number(port || 443), servername: hostname });
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error(`timed out after ${TIMEOUT_MS / 1000}s`));
    }, TIMEOUT_MS);
    socket.once('secureConnect', () => {
      clearTimeout(timer);
      socket.end();
      resolvePromise(socket.getPeerCertificate()?.subject?.CN ?? '?');
    });
    socket.once('error', (error) => {
      clearTimeout(timer);
      reject(error);
    });
  });
}

async function twitchDefaultUrl() {
  const response = await fetch('https://ingest.twitch.tv/ingests', {
    signal: AbortSignal.timeout(TIMEOUT_MS)
  });
  if (!response.ok) throw new Error(`ingest API answered ${response.status}`);
  const { ingests } = await response.json();
  const template = ingests?.find((ingest) => ingest._id === 0)?.url_template_secure;
  if (!template) throw new Error('ingest API lists no default secure ingest');
  return template.replace(/\/\{stream_key\}$/, '');
}

let failures = 0;
console.log(`Stream presets last verified ${verifiedOn ?? '(unknown)'}; checking ${presets.length}…`);
if (presets.length === 0) {
  console.error('  no presets found -- did streamServices.ts change shape?');
  process.exit(1);
}
for (const { id, url } of presets) {
  try {
    const cn = await handshake(url);
    console.log(`  ok    ${id.padEnd(9)} ${url}  (certificate ${cn})`);
  } catch (error) {
    failures += 1;
    console.error(`  FAIL  ${id.padEnd(9)} ${url}  ${error.message}`);
  }
}
const twitch = presets.find((preset) => preset.id === 'twitch');
if (twitch) {
  try {
    const expected = await twitchDefaultUrl();
    if (expected === twitch.url) {
      console.log('  ok    twitch    matches the ingest API default');
    } else {
      failures += 1;
      console.error(`  FAIL  twitch    ingest API now hands out ${expected}`);
    }
  } catch (error) {
    failures += 1;
    console.error(`  FAIL  twitch    ingest API: ${error.message}`);
  }
}
if (failures > 0) {
  console.error(`${failures} check(s) failed: update src/lib/data/streamServices.ts.`);
  process.exit(1);
}
console.log('All presets answer. Bump STREAM_SERVICES_VERIFIED_ON if this was a scheduled check.');
