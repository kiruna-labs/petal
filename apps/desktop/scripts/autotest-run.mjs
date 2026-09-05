#!/usr/bin/env node
import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import process from 'node:process';

function usage() {
  console.error('Usage: node scripts/autotest-run.mjs <scenario.json> [socketPath]');
  process.exit(2);
}

const scenarioPath = process.argv[2];
const socketPath = process.argv[3] || process.env.PETAL_AUTOTEST_SOCK;
if (!scenarioPath || !socketPath) usage();

const scenario = JSON.parse(fs.readFileSync(scenarioPath, 'utf8'));
const commands = scenario.commands ?? [];
if (!Array.isArray(commands)) throw new Error('scenario.commands must be an array');

const socket = net.createConnection(socketPath);
let buffer = '';
let pendingResolve;
let pendingReject;
let failed = false;

socket.setEncoding('utf8');
socket.on('data', (chunk) => {
  buffer += chunk;
  let idx;
  while ((idx = buffer.indexOf('\n')) >= 0) {
    const line = buffer.slice(0, idx);
    buffer = buffer.slice(idx + 1);
    if (!line.trim()) continue;
    const response = JSON.parse(line);
    const resolve = pendingResolve;
    pendingResolve = undefined;
    pendingReject = undefined;
    resolve?.(response);
  }
});
socket.on('error', (err) => {
  if (pendingReject) pendingReject(err);
  else throw err;
});

function send(command) {
  return new Promise((resolve, reject) => {
    pendingResolve = resolve;
    pendingReject = reject;
    socket.write(`${JSON.stringify(command)}\n`);
  });
}

function assertResponse(step, response) {
  if (step.expectOk !== false && !response.ok) {
    throw new Error(`step ${step.name ?? step.cmd} failed: ${response.error}`);
  }
  if (step.expectOk === false && response.ok) {
    throw new Error(`step ${step.name ?? step.cmd} unexpectedly succeeded`);
  }
  if (step.expect) {
    for (const [key, expected] of Object.entries(step.expect)) {
      const actual = key.split('.').reduce((value, part) => value?.[part], response);
      if (JSON.stringify(actual) !== JSON.stringify(expected)) {
        throw new Error(
          `step ${step.name ?? step.cmd} expected ${key}=${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`
        );
      }
    }
  }
}

function valueAtPath(value, dottedPath) {
  return dottedPath.split('.').reduce((current, part) => current?.[part], value);
}

function predicateMatches(actual, predicate) {
  if (typeof predicate === 'undefined') return Boolean(actual);
  if (Array.isArray(predicate)) return predicate.some((expected) => predicateMatches(actual, expected));
  if (predicate && typeof predicate === 'object') {
    if ('equals' in predicate) return JSON.stringify(actual) === JSON.stringify(predicate.equals);
    if ('notEquals' in predicate) return JSON.stringify(actual) !== JSON.stringify(predicate.notEquals);
    if ('gt' in predicate) return Number(actual) > Number(predicate.gt);
    if ('gte' in predicate) return Number(actual) >= Number(predicate.gte);
    if ('lt' in predicate) return Number(actual) < Number(predicate.lt);
    if ('lte' in predicate) return Number(actual) <= Number(predicate.lte);
    if ('includes' in predicate) return Array.isArray(actual) && actual.includes(predicate.includes);
  }
  return JSON.stringify(actual) === JSON.stringify(predicate);
}

async function pollUntil(step) {
  const {
    name,
    expr,
    timeoutMs = 5000,
    intervalMs = 250,
    expect,
    expectOk,
    type,
    sleepMs,
    ...command
  } = step;
  if (!expr || typeof expr !== 'object' || typeof expr.path !== 'string') {
    throw new Error(`step ${name ?? 'pollUntil'} requires expr: { path, ...predicate }`);
  }
  if (!command.cmd) {
    throw new Error(`step ${name ?? 'pollUntil'} requires a socket command`);
  }

  const deadline = Date.now() + timeoutMs;
  let lastResponse;
  let lastValue;
  let attempts = 0;
  while (Date.now() <= deadline) {
    attempts += 1;
    lastResponse = await send(command);
    assertResponse({ ...step, expect, expectOk }, lastResponse);
    lastValue = valueAtPath(lastResponse, expr.path);
    if (predicateMatches(lastValue, expr)) {
      console.log(
        `ok ${name ?? command.cmd}: ${JSON.stringify(lastResponse.result ?? lastResponse.error)} (${attempts} poll${attempts === 1 ? '' : 's'})`
      );
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, intervalMs));
  }

  throw new Error(
    `step ${name ?? command.cmd} timed out after ${timeoutMs}ms waiting for ${expr.path}; last value=${JSON.stringify(lastValue)}, last response=${JSON.stringify(lastResponse)}`
  );
}

try {
  console.log(`# ${scenario.name ?? path.basename(scenarioPath)}`);
  for (const step of commands) {
    if (step.sleepMs) {
      await new Promise((resolve) => setTimeout(resolve, step.sleepMs));
      continue;
    }
    if (step.type === 'pollUntil') {
      await pollUntil(step);
      continue;
    }
    const { name, expect, expectOk, sleepMs, ...command } = step;
    const response = await send(command);
    assertResponse(step, response);
    console.log(`ok ${name ?? command.cmd}: ${JSON.stringify(response.result ?? response.error)}`);
  }
} catch (err) {
  failed = true;
  console.error(`not ok: ${err.message}`);
} finally {
  socket.end();
}

process.exit(failed ? 1 : 0);
