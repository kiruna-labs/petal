#!/usr/bin/env node
// Bounded client for the native app's autotest command socket (#102).
//
// Extracted from `remote-control-scenario.mjs` so the timeout below is
// testable against a server that deliberately never answers -- which is
// exactly the failure this module exists to bound.
//
// THE DEFECT: the original client resolved a command's promise only when a
// newline-terminated response arrived, and rejected only on a socket `error`.
// It handled neither a timeout nor `close`/`end`. So when the app stopped
// answering while the socket stayed open -- which is what happened during the
// #298 resume/reconnect simulation on 2026-09-08 -- `await command(...)` never
// settled, the suite blocked forever, and one wedged run held the only
// self-hosted macOS runner for 81 minutes with a release gate queued behind it.
//
// Every command is now bounded. On a timeout the socket is destroyed and every
// later command fails immediately with the same reason: once the app has
// stopped answering, nothing after it proves anything, and reporting 30 loud
// failures beats hanging.
import net from 'node:net';
import process from 'node:process';

import { captureWedgeEvidence } from './harness-wedge-evidence.mjs';
import { commandTimeoutMs } from './harness-timeouts.mjs';

export function connectSocket(file, options = {}) {
  const timeoutMs = options.timeoutMs ?? commandTimeoutMs();
  const onWedge = options.onWedge ?? ((reason) => captureWedgeEvidence(reason));
  const socket = options.socket ?? net.createConnection(file);
  socket.setEncoding('utf8');
  let buffer = '';
  let pending;
  // Set once the socket is unusable: a timed-out command leaves the stream
  // out of sync (a late reply would be handed to the NEXT command), and a
  // closed socket can never answer. Both are terminal for this client.
  let deadReason = null;

  function settleDead(reason) {
    deadReason = reason;
    const waiting = pending;
    pending = undefined;
    waiting?.reject(new Error(reason));
  }

  socket.on('data', (chunk) => {
    buffer += chunk;
    let idx;
    while ((idx = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, idx);
      buffer = buffer.slice(idx + 1);
      if (!line.trim()) continue;
      const waiting = pending;
      pending = undefined;
      waiting?.settle(JSON.parse(line));
    }
  });
  socket.on('error', (error) => {
    settleDead(`autotest socket error: ${error.message}`);
  });
  // #102: neither of these was handled before. A peer that closes cleanly
  // while a command is in flight produced no `error`, so the promise simply
  // never settled.
  socket.on('close', () => {
    if (!deadReason) settleDead('autotest socket closed by the app before it answered');
  });
  socket.on('end', () => {
    if (!deadReason) settleDead('autotest socket ended by the app before it answered');
  });

  return {
    send(command) {
      return new Promise((resolve, reject) => {
        if (deadReason) {
          reject(new Error(`${deadReason} (refusing '${command.cmd}': the socket is no longer usable)`));
          return;
        }
        if (pending) {
          reject(new Error('autotest socket only supports one in-flight command'));
          return;
        }
        const timer = setTimeout(() => {
          const reason =
            `autotest command '${command.cmd}' got no response within ${timeoutMs}ms -- ` +
            'the app stopped answering the autotest socket (#102)';
          // Evidence FIRST, while the app is still wedged: a backtrace taken
          // after teardown proves nothing.
          try {
            onWedge(reason);
          } catch {
            // Never let evidence capture mask the wedge itself.
          }
          settleDead(reason);
          socket.destroy();
        }, timeoutMs);
        // `unref` so a pending timer can never be the reason the harness
        // process itself refuses to exit.
        timer.unref?.();
        pending = {
          settle(response) {
            clearTimeout(timer);
            resolve(response);
          },
          reject(error) {
            clearTimeout(timer);
            reject(error);
          },
        };
        socket.write(`${JSON.stringify(command)}\n`);
      });
    },
    close() {
      socket.end();
    },
  };
}
