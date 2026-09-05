#!/usr/bin/env bash
# Regenerate the committed CycloneDX SBOMs under sbom/.
#
# One SBOM per dependency root:
#   sbom/desktop-rust.cdx.json   apps/desktop/src-tauri  (cargo cyclonedx)
#   sbom/desktop-npm.cdx.json    apps/desktop            (npm sbom)
#   sbom/backend-npm.cdx.json    backend                 (npm sbom)
#   sbom/web-harness-npm.cdx.json web-harness            (npm sbom)
#   sbom/site-npm.cdx.json       site                    (npm sbom)
#
# `shared/` has no manifest of its own (it is consumed by path from
# apps/desktop and web-harness and declares no dependencies), so it has no
# SBOM; its code is first-party and covered by LICENSE.
#
# Output is normalised so that a re-run on an unchanged lockfile is
# byte-identical: timestamps, per-run serial numbers and tool-version
# metadata are stripped. .github/workflows/sbom.yml regenerates and fails
# on any diff, so a lockfile change must land with its SBOM.
#
# Prereqs: cargo-cyclonedx 0.5.x (cargo install cargo-cyclonedx), npm 11
# (npm sbom is built in), node_modules installed in each npm root
# (`npm ci --ignore-scripts` is enough -- no native build needed).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/sbom"
mkdir -p "$OUT"

normalise() {
  # $1 = path. Strip run-varying metadata; sort keys for stable diffs.
  node -e '
    const fs = require("fs");
    const p = process.argv[1];
    const root = process.argv[2];
    // cargo-cyclonedx records path dependencies (vendor/*) as
    // path+file://<absolute checkout path>; rewrite to a checkout-relative
    // form so the file is identical on every machine (this is what broke the
    // first sbom.yml run: /Users/... locally vs /home/runner/... in CI).
    let raw = fs.readFileSync(p, "utf8")
      .split("path+file://" + root).join("path+file:///petal")
      .split("download_url=file://" + root).join("download_url=file:///petal");
    const j = JSON.parse(raw);
    delete j.serialNumber;
    // npm 11 adds fields npm 10 does not (root externalReferences from
    // package.json#repository, cdx:npm:package:path); drop the npm-version-
    // dependent ones so the diff gate compares dependency content only.
    if (j.metadata && j.metadata.component) delete j.metadata.component.externalReferences;
    const dropProps = (c) => {
      if (Array.isArray(c.properties)) {
        c.properties = c.properties.filter((x) => x.name !== "cdx:npm:package:path");
        if (!c.properties.length) delete c.properties;
      }
    };
    if (j.metadata && j.metadata.component) dropProps(j.metadata.component);
    (j.components || []).forEach(dropProps);
    if (j.metadata) {
      delete j.metadata.timestamp;
      // cargo-cyclonedx / npm record their own version here; not a dependency.
      if (j.metadata.tools) {
        const t = j.metadata.tools;
        const strip = (x) => { delete x.version; return x; };
        if (Array.isArray(t)) j.metadata.tools = t.map(strip);
        else if (t.components) t.components = t.components.map(strip);
      }
    }
    // Drop per-person contact fields (crate/package author e-mails). They
    // are public on the registries, but the public-tree PII gate
    // (scripts/scan-for-pii) rightly refuses to ship e-mail addresses, and
    // an SBOM only needs name/version/purl/license to be useful.
    const stripPeople = (v) => {
      if (Array.isArray(v)) return v.map(stripPeople);
      if (v && typeof v === "object") {
        for (const k of ["author", "authors", "publisher", "supplier", "manufacture"]) delete v[k];
        for (const k of Object.keys(v)) v[k] = stripPeople(v[k]);
      }
      return v;
    };
    stripPeople(j);
    const sortKeys = (v) => Array.isArray(v) ? v.map(sortKeys)
      : (v && typeof v === "object")
        ? Object.fromEntries(Object.keys(v).sort().map(k => [k, sortKeys(v[k])]))
        : v;
    fs.writeFileSync(p, JSON.stringify(sortKeys(j), null, 2) + "\n");
  ' "$1" "$ROOT"
}

npm_sbom() {
  # $1 = dir, $2 = output name
  local dir="$ROOT/$1" out="$OUT/$2"
  if [ ! -d "$dir/node_modules" ]; then
    echo "generate-sbom: $1/node_modules missing -- run 'npm ci --ignore-scripts' there first" >&2
    exit 1
  fi
  (cd "$dir" && npm sbom --sbom-format cyclonedx --package-lock-only > "$out")
  normalise "$out"
  echo "wrote $2 ($(node -e 'console.log(JSON.parse(require("fs").readFileSync(process.argv[1])).components.length)' "$out") components)"
}

# --- Rust -------------------------------------------------------------------
if ! cargo cyclonedx --version >/dev/null 2>&1; then
  echo "generate-sbom: cargo-cyclonedx not installed (cargo install cargo-cyclonedx)" >&2
  exit 1
fi
# --all is the full transitive graph (not just top-level deps); --target all
# includes every platform's deps (Windows crates too), not just the host. cargo-cyclonedx
# writes <override-filename>.json next to the manifest; move it into sbom/.
(cd "$ROOT/apps/desktop/src-tauri" \
  && cargo cyclonedx --format json --all --target all --override-filename desktop-rust >/dev/null)
mv "$ROOT/apps/desktop/src-tauri/desktop-rust.json" "$OUT/desktop-rust.cdx.json"
normalise "$OUT/desktop-rust.cdx.json"
echo "wrote desktop-rust.cdx.json ($(node -e 'console.log(JSON.parse(require("fs").readFileSync(process.argv[1])).components.length)' "$OUT/desktop-rust.cdx.json") components)"

# --- npm --------------------------------------------------------------------
npm_sbom apps/desktop desktop-npm.cdx.json
npm_sbom backend     backend-npm.cdx.json
npm_sbom web-harness web-harness-npm.cdx.json
npm_sbom site        site-npm.cdx.json

echo "generate-sbom: done -> $OUT"
