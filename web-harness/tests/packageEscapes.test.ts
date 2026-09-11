// #93: proves `scripts/check-package-escapes.mjs` fires in BOTH directions --
// it accepts the real package AND rejects a relative import that walks out of
// it. A gate whose negative case is never exercised is worth nothing (CLAUDE.md
// "Test a gate in BOTH directions before relying on it"): this one exists
// because a fixture reaching `../../../../shared/...` turned into a TS2307
// inside the isolated-deploy simulation (#662) and made ci-local.sh red on main.
import { strict as assert } from 'node:assert';
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const GUARD = fileURLToPath(new URL('../scripts/check-package-escapes.mjs', import.meta.url));
const PACKAGE_ROOT = fileURLToPath(new URL('..', import.meta.url));

function runGuard(root: string) {
  const result = spawnSync(process.execPath, [GUARD, root], { encoding: 'utf8' });
  return { status: result.status, output: `${result.stdout}${result.stderr}` };
}

// The guard reads source text, so it cannot tell an import from a string that
// LOOKS like one -- this file would otherwise flag its own fixtures. Build every
// fixture line through this helper: the specifier never sits next to a literal
// `from` in this file's own source.
function importLine(binding: string, specifier: string): string {
  const quoted = `'${specifier}'`;
  return binding ? `import ${binding} from ${quoted};` : `import ${quoted};`;
}

/** A throwaway package that mimics web-harness's shape: pkg/tests/fixtures/plugins/. */
function stagePackage(fixtureSource: string): string {
  const root = mkdtempSync(join(tmpdir(), 'petal-escape-guard.'));
  const dir = join(root, 'tests', 'fixtures', 'plugins');
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, 'fixture.ts'), fixtureSource);
  return root;
}

test('the real web-harness package has no relative import that escapes it', () => {
  const { status, output } = runGuard(PACKAGE_ROOT);
  assert.equal(status, 0, output);
  assert.match(output, /no relative import escapes/);
});

test('an escaping relative import to shared/ is rejected with an actionable message', () => {
  const root = stagePackage(
    [
      importLine('{ createPluginHost }', '../../../../shared/plugin-host/host.ts'),
      'export const host = createPluginHost;',
      '',
    ].join('\n')
  );
  try {
    const { status, output } = runGuard(root);
    assert.equal(status, 1, output);
    assert.match(output, /PACKAGE-ESCAPE GATE BLOCKED/);
    assert.match(output, /tests[\\/]fixtures[\\/]plugins[\\/]fixture\.ts:1/);
    assert.match(output, /\.\.\/\.\.\/\.\.\/\.\.\/shared\/plugin-host\/host\.ts/);
    // The message has to name the fix, not just the offence.
    assert.match(output, /@petal\/shared/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('the supported routes -- the @petal/shared alias and in-package relatives -- pass', () => {
  const root = stagePackage(
    [
      importLine('{ createPluginHost }', '@petal/shared/plugin-host/host.ts'),
      importLine('', '@petal/shared/ui/tokens.css'),
      // ../../../shared from tests/fixtures/plugins/ is web-harness/shared, the
      // symlink `rsync -L` materializes in the staged copy: in-package, fine.
      importLine('{ validateManifest }', '../../../shared/plugin-host/manifest.ts'),
      importLine('', '../../../src/fonts.css'),
      'export const host = { createPluginHost, validateManifest };',
      '',
    ].join('\n')
  );
  try {
    const { status, output } = runGuard(root);
    assert.equal(status, 0, output);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('a commented-out escaping import does not fail the build', () => {
  const root = stagePackage(
    [
      `// ${importLine('{ createPluginHost }', '../../../../shared/plugin-host/host.ts')}`,
      `/* ${importLine('', '../../../../shared/ui/tokens.css')} */`,
      'export const nothing = true;',
      '',
    ].join('\n')
  );
  try {
    const { status, output } = runGuard(root);
    assert.equal(status, 0, output);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
