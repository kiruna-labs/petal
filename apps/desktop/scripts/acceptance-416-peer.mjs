#!/usr/bin/env node
// Drive the browser peer for the #416 acceptance loop over CDP: join the room,
// publish the test-pattern canvas, and (optionally) change the published
// resolution -- i.e. everything the receiver-side harness needs the SOURCE to
// do. Kept separate from acceptance-416.mjs so the handle sweep can use the
// same peer without re-running the measurement.
//
// Usage:
//   acceptance-416-peer.mjs join <accessCode>
//   acceptance-416-peer.mjs share
//   acceptance-416-peer.mjs size <w> <h>
//   acceptance-416-peer.mjs state

import http from 'node:http';
import process from 'node:process';

const cdpListUrl = process.env.PETAL_REMOTE_CONTROL_CDP_JSON || 'http://127.0.0.1:9222/json';
const urlNeedle = process.env.PETAL_WEB_HARNESS_URL_MATCH || 'localhost:5185';

function httpJson(url) {
  return new Promise((resolve, reject) => {
    http
      .get(url, (response) => {
        let body = '';
        response.on('data', (chunk) => (body += chunk));
        response.on('end', () => {
          try {
            resolve(JSON.parse(body));
          } catch (error) {
            reject(error);
          }
        });
      })
      .on('error', reject);
  });
}

async function connect() {
  const pages = await httpJson(cdpListUrl);
  const page = pages.find((entry) => entry.type === 'page' && String(entry.url).includes(urlNeedle));
  if (!page) throw new Error(`no CDP page matching ${urlNeedle}`);
  const socket = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    socket.onopen = resolve;
    socket.onerror = reject;
  });
  let nextId = 1;
  const pending = new Map();
  socket.onmessage = (message) => {
    const payload = JSON.parse(typeof message.data === 'string' ? message.data : message.data.toString());
    const entry = pending.get(payload.id);
    if (!entry) return;
    pending.delete(payload.id);
    if (payload.error) entry.reject(new Error(payload.error.message));
    else entry.resolve(payload.result);
  };
  return {
    send(method, params) {
      const id = nextId++;
      return new Promise((resolve, reject) => {
        pending.set(id, { resolve, reject });
        socket.send(JSON.stringify({ id, method, params }));
      });
    },
    close: () => socket.close(),
  };
}

async function evaluate(cdp, expression) {
  const result = await cdp.send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
  if (result.exceptionDetails) {
    throw new Error(result.exceptionDetails.exception?.description || result.exceptionDetails.text || 'eval failed');
  }
  return result.result?.value;
}

const [action, ...rest] = process.argv.slice(2);
const cdp = await connect();
try {
  if (action === 'join') {
    const code = rest[0];
    console.log(
      JSON.stringify(
        await evaluate(
          cdp,
          `(async () => {
            const hook = window.__petalHarness?.cockpitAutoScenario;
            if (!hook) throw new Error('cockpitAutoScenario hook unavailable');
            return await hook.join(${JSON.stringify(code)});
          })()`
        )
      )
    );
  } else if (action === 'share') {
    console.log(
      JSON.stringify(
        await evaluate(
          cdp,
          `(async () => {
            const hook = window.__petalHarness?.cockpitAutoScenario;
            if (!hook) throw new Error('cockpitAutoScenario hook unavailable');
            return await hook.sharePattern();
          })()`
        )
      )
    );
  } else if (action === 'size') {
    const w = Number(rest[0]);
    const h = Number(rest[1]);
    console.log(
      JSON.stringify(
        await evaluate(
          cdp,
          `(() => {
            const canvas = document.querySelector('canvas');
            if (!canvas) throw new Error('no shared canvas in the web peer');
            canvas.width = ${w};
            canvas.height = ${h};
            const ctx = canvas.getContext('2d');
            if (ctx) { ctx.fillStyle = '#1b1033'; ctx.fillRect(0, 0, ${w}, ${h}); }
            return { w: canvas.width, h: canvas.height };
          })()`
        )
      )
    );
  } else if (action === 'source-resize') {
    // A GENUINE sender-side logical-size change, as the RECEIVER defines one.
    //
    // Two things make the obvious approaches no-ops, and both silently produce
    // a race harness that races nothing:
    //   1. Resizing the shared canvas does not change the source geometry the
    //      receiver uses. `canonical_source_size_for_frame` pins that to the
    //      publisher's dimensions captured at subscribe time and explicitly
    //      refuses to let decoded frame sizes redefine it, so that a simulcast
    //      downswitch cannot shrink the panel. Only `ensure_window`'s reuse
    //      branch -> `update_canonical_source_size_on_republish` updates it.
    //   2. Clicking the dev-panel share button to republish does not help
    //      either: `startTestPatternShare` calls `startCanvasAnimation` ->
    //      `prepareTestPatternCanvas`, which forces the canvas back to
    //      960x600 every time. Any width set beforehand is reverted before
    //      `captureStream` is called.
    // So publish the resized canvas directly, reusing the page's own already-
    // published track object for its class, name and source -- same track
    // name, same window id, same publish contract, new dimensions.
    const w = Number(rest[0]);
    const h = Number(rest[1]);
    console.log(
      JSON.stringify(
        await evaluate(
          cdp,
          `(async () => {
            const hook = window.__petalHarness;
            const room = hook?.room;
            const old = hook?.localVideoTrack;
            if (!room || !old) throw new Error('web peer is not publishing a window track');
            const publication = [...room.localParticipant.videoTrackPublications.values()]
              .find((p) => p.track === old);
            const trackName = publication?.trackName || old.name;
            if (!trackName) throw new Error('could not resolve the published track name');
            const canvas = document.querySelector('canvas');
            if (!canvas) throw new Error('no shared canvas in the web peer');
            canvas.width = ${w};
            canvas.height = ${h};
            const media = canvas.captureStream(30).getVideoTracks()[0];
            media.contentHint = 'detail';
            const LocalVideoTrack = old.constructor;
            const next = new LocalVideoTrack(media);
            await room.localParticipant.unpublishTrack(old, true);
            await room.localParticipant.publishTrack(next, {
              name: trackName,
              source: old.source,
              videoCodec: 'h264'
            });
            hook.localVideoTrack = next;
            return { trackName, w: canvas.width, h: canvas.height };
          })()`
        )
      )
    );
  } else if (action === 'republish') {
    // A GENUINE source-side logical-size change, as the receiver defines one.
    // Resizing the canvas alone is NOT one: `canonical_source_size_for_frame`
    // pins the source geometry to the publisher's dimensions captured when the
    // track was subscribed and explicitly refuses to let decoded frame sizes
    // redefine it (so a simulcast downswitch cannot shrink the panel). The only
    // mid-session path that updates it is `ensure_window`'s reuse branch ->
    // `update_canonical_source_size_on_republish`, reached when the sharer
    // republishes the track. So: resize the canvas, then stop and restart the
    // share through the real UI button, under the page's stable window id.
    const w = Number(rest[0]);
    const h = Number(rest[1]);
    console.log(
      JSON.stringify(
        await evaluate(
          cdp,
          `(async () => {
            const canvas = document.querySelector('canvas');
            if (!canvas) throw new Error('no shared canvas in the web peer');
            canvas.width = ${w};
            canvas.height = ${h};
            const button = document.querySelector('#share-btn');
            if (!button) throw new Error('no #share-btn');
            const wasSharing = /stop/i.test(button.textContent || '');
            if (wasSharing) {
              button.click();
              await new Promise((r) => setTimeout(r, 250));
            }
            button.click();
            await new Promise((r) => setTimeout(r, 250));
            return { w: canvas.width, h: canvas.height, label: button.textContent };
          })()`
        )
      )
    );
  } else {
    console.log(
      JSON.stringify(
        await evaluate(
          cdp,
          `(() => ({
            url: location.href,
            hook: !!window.__petalHarness?.cockpitAutoScenario,
            canvases: [...document.querySelectorAll('canvas')].map((c) => ({ w: c.width, h: c.height })),
            text: document.body.innerText.slice(0, 600)
          }))()`
        ),
        null,
        2
      )
    );
  }
} finally {
  cdp.close();
}
