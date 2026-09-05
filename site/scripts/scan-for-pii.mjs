#!/usr/bin/env node
// PII / secrets CI gate for the Petal docs site (issue #437).
//
// Scans the BUILT site output (site/dist/ by default) for a deny-list of
// patterns that must never appear on a public docs site: personal email
// addresses, this project's keychain-profile name, and generic secret-shaped
// tokens. Exits non-zero and prints file/line + a redacted context snippet
// for every hit; exits 0 with a summary when the scan is clean.
//
// This is a hard CI gate (see scripts/ci-local.sh), not a suggestion — the
// project's non-negotiable rule is that this site never publishes personal
// or individually-identifying operational detail.
import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { extname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const SITE_ROOT = resolve(fileURLToPath(import.meta.url), '..', '..');
const DEFAULT_TARGET = join(SITE_ROOT, 'dist');

// Only scan text-ish output; skip binary assets (images, fonts) where a scan
// is meaningless and can false-positive on random bytes.
const SCANNABLE_EXTENSIONS = new Set([
	'.html',
	'.htm',
	'.md',
	'.mdx',
	'.txt',
	'.xml',
	'.json',
	'.js',
	'.mjs',
	'.css',
	'.svg',
	// Source and config, so scripts/scan-public-tree.sh covers the whole
	// exportable tree rather than just rendered docs. A personal address in a
	// Rust test fixture is exactly as public as one in a built HTML page.
	'.rs',
	'.ts',
	'.tsx',
	'.svelte',
	'.toml',
	'.yml',
	'.yaml',
	'.sh',
	'.cjs',
	'.env',
	'.example',
	'.plist',
	'.swift',
]);

// Compiled/bundled build-tool output that never contains OUR authored
// content — it's either Astro/Starlight's own client bundle or Pagefind's
// vendored search-UI library, which ships upstream contributor credits
// (real emails, but not this project's) baked into its minified JS. Our
// actual page content is fully covered by scanning the rendered HTML pages
// (and Pagefind's fragment/index files, which are non-text formats already
// excluded by SCANNABLE_EXTENSIONS above), so excluding these directories
// doesn't reduce coverage of anything we wrote.
//
// Matched with `(^|/)` rather than anchored to the scan root: astro.config.mjs
// sets `outDir: './dist/docs'` (petal.live/docs deploy, #437) so these
// directories now land one level deeper (dist/docs/_astro/,
// dist/docs/pagefind/...) instead of directly under dist/. The pattern must
// match regardless of how many path segments precede it.
//
// Third-party content we redistribute but did not author is skipped for the
// same reason: upstream crate manifests carry their own authors' addresses,
// lockfiles carry maintainer funding contacts, and vendored source uses words
// that collide with our deny-list (LiveKit's ICE "trickle" is not our notary
// profile). Rewriting any of it would corrupt the vendored copy, and none of
// it is this project's PII.
const SKIP_PATH_PATTERNS = [
	/(^|\/)_astro\//,
	/(^|\/)pagefind\/pagefind[^/]*\.(js|css)$/,
	/(^|\/)vendor\//,
	/(^|\/)package-lock\.json$/,
	/(^|\/)node_modules\//,
];

function isSkipped(relPath) {
	return SKIP_PATH_PATTERNS.some((pattern) => pattern.test(relPath));
}

/**
 * Each pattern is checked against every scannable file. `redact` controls
 * how the matched substring is shown in the report — never the raw match
 * itself for anything secret-shaped.
 */
// Addresses that are deliberately non-deliverable or deliberately public, and
// so are never a PII leak. RFC 2606 reserves example.com/.org/.net and the
// .example/.invalid/.test TLDs precisely for documentation and test fixtures;
// GitHub noreply addresses are the pseudonymous commit identity we WANT used.
const ALLOWED_EMAIL_PATTERNS = [
	/@(?:[A-Za-z0-9-]+\.)*example\.[A-Za-z.]{2,}$/i,
	/@(?:[A-Za-z0-9-]+\.)*(?:example|invalid|test|localhost)$/i,
	/@users\.noreply\.github\.com$/i,
	/^noreply@/i,
	// Sentry DSNs are deliberately public (embedded in shipped client-side
	// binaries/JS by design -- see init_sentry() and the web build's
	// VITE_SENTRY_DSN) -- not credentials, and not a personal address. They
	// just happen to satisfy the generic mailbox shape (<key>@<ingest-host>).
	/@[A-Za-z0-9-]+\.ingest\.[a-z0-9.-]*sentry\.io$/i,
	// Not an address at all: `icons/128x128@2x.png` and friends satisfy the
	// mailbox shape. Anything whose "domain" part ends in a file extension is a
	// path, not a mailbox.
	/\.(?:png|jpe?g|gif|webp|svg|ico|icns|woff2?|ttf|otf|css|js|mjs|ts|json|md|txt|html?|zip|dmg)$/i,
];

function isAllowedEmail(match) {
	return ALLOWED_EMAIL_PATTERNS.some((pattern) => pattern.test(match));
}

const DENY_PATTERNS = [
	{
		name: 'personal-email-address',
		// Generic email-address shape. Deliberately broad: it must catch ANY
		// real address, not a specific hardcoded one. Documentation/test
		// placeholders are excluded via `allow` rather than by narrowing this.
		regex: /[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/g,
		allow: isAllowedEmail,
		redact: () => '[REDACTED]',
	},
	{
		name: 'openai-style-secret-key',
		regex: /\bsk-[A-Za-z0-9]{16,}\b/g,
		redact: () => '[REDACTED]',
	},
	{
		name: 'github-token',
		regex: /\bgh[pousr]_[A-Za-z0-9]{20,}\b/g,
		redact: () => '[REDACTED]',
	},
	{
		name: 'aws-access-key-id',
		regex: /\bAKIA[0-9A-Z]{16}\b/g,
		redact: () => '[REDACTED]',
	},
	{
		name: 'google-api-key',
		regex: /\bAIza[0-9A-Za-z_-]{35}\b/g,
		redact: () => '[REDACTED]',
	},
	{
		name: 'slack-token',
		regex: /\bxox[baprs]-[A-Za-z0-9-]{10,}\b/g,
		redact: () => '[REDACTED]',
	},
	{
		name: 'private-key-block',
		regex: /-----BEGIN (?:RSA |EC |OPENSSH |DSA |)PRIVATE KEY-----/g,
		redact: () => '[REDACTED]',
	},
	{
		name: 'credentialed-connection-string',
		// Flags any URL carrying inline credentials, not just one database's.
		regex: /\b[a-z][a-z0-9+.-]*:\/\/[^\s/:@]+:[^\s/:@]+@[^\s/'"]+/gi,
		redact: () => '[REDACTED]',
	},
	...loadLocalPatterns(),
];

/**
 * Project-specific literals (a maintainer's real name, a notarization
 * keychain-profile name, an operator handle) must NOT live in this file —
 * writing them here publishes exactly what the scanner exists to suppress, and
 * this script is itself part of a public repository.
 *
 * Instead they come from an untracked local file, one JSON object per entry:
 *   [{ "name": "known-personal-name", "pattern": "Jane\\s+Doe", "flags": "gi" }]
 *
 * Default path `site/.pii-patterns.local.json` (gitignored); override with
 * PETAL_PII_PATTERNS_FILE. Absent file = generic patterns only, which is the
 * correct default for an outside contributor.
 */
function loadLocalPatterns() {
	const file =
		process.env.PETAL_PII_PATTERNS_FILE ?? join(SITE_ROOT, '.pii-patterns.local.json');
	if (!existsSync(file)) return [];
	let entries;
	try {
		entries = JSON.parse(readFileSync(file, 'utf8'));
	} catch (error) {
		throw new Error(`PII pattern file ${file} is not valid JSON: ${error.message}`);
	}
	if (!Array.isArray(entries)) throw new Error(`PII pattern file ${file} must contain an array`);
	return entries.map(({ name, pattern, flags }) => {
		if (!name || !pattern) throw new Error(`PII pattern file ${file}: each entry needs name+pattern`);
		return {
			name: `local:${name}`,
			regex: new RegExp(pattern, flags?.includes('g') ? flags : `${flags ?? ''}g`),
			redact: () => '[REDACTED]',
		};
	});
}

function walk(root, dir = root, files = []) {
	for (const entry of readdirSync(dir)) {
		const full = join(dir, entry);
		const relToRoot = relative(root, full);
		if (isSkipped(relToRoot)) continue;
		const info = statSync(full);
		if (info.isDirectory()) {
			walk(root, full, files);
		} else if (SCANNABLE_EXTENSIONS.has(extname(entry).toLowerCase())) {
			files.push(full);
		}
	}
	return files;
}

function redactedContext(line, matchStart, matchEnd, redacted) {
	const CONTEXT = 24;
	const start = Math.max(0, matchStart - CONTEXT);
	const end = Math.min(line.length, matchEnd + CONTEXT);
	const before = line.slice(start, matchStart);
	const after = line.slice(matchEnd, end);
	return `${start > 0 ? '…' : ''}${before}${redacted}${after}${end < line.length ? '…' : ''}`;
}

function scanFile(path) {
	const text = readFileSync(path, 'utf8');
	const lines = text.split('\n');
	const hits = [];
	lines.forEach((line, idx) => {
		for (const pattern of DENY_PATTERNS) {
			pattern.regex.lastIndex = 0;
			let match;
			while ((match = pattern.regex.exec(line))) {
				if (match[0].length === 0) {
					pattern.regex.lastIndex++;
					continue;
				}
				if (pattern.allow?.(match[0])) continue;
				const redacted = pattern.redact(match[0]);
				hits.push({
					line: idx + 1,
					pattern: pattern.name,
					context: redactedContext(line, match.index, match.index + match[0].length, redacted),
				});
			}
		}
	});
	return hits;
}

function main() {
	const target = resolve(process.argv[2] ?? DEFAULT_TARGET);
	let files;
	try {
		files = walk(target);
	} catch (err) {
		console.error(`[scan-for-pii] Cannot read target directory: ${target}`);
		console.error(`[scan-for-pii] Did you run \`npm run build\` first? (${err.message})`);
		process.exit(1);
	}

	if (files.length === 0) {
		console.error(`[scan-for-pii] No scannable files found under ${target} — refusing to report a false "clean".`);
		process.exit(1);
	}

	let totalHits = 0;
	for (const file of files) {
		const hits = scanFile(file);
		if (hits.length > 0) {
			totalHits += hits.length;
			console.error(`\n${relative(SITE_ROOT, file)}`);
			for (const hit of hits) {
				console.error(`  line ${hit.line} [${hit.pattern}]: ${hit.context}`);
			}
		}
	}

	console.log(`\n[scan-for-pii] scanned ${files.length} file(s) under ${relative(SITE_ROOT, target)}.`);
	if (totalHits > 0) {
		console.error(`[scan-for-pii] FAILED — ${totalHits} potential PII/secret hit(s) found. See above.`);
		process.exit(1);
	}
	console.log('[scan-for-pii] OK — zero hits.');
}

main();
