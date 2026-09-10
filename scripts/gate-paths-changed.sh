#!/usr/bin/env bash
#
# Decide whether a pull request touches any of a gate's paths.
#
# WHY THIS EXISTS (#133). The PR gates used to be two workflows per gate: a
# real one with `paths:` and a no-op companion with the same list under
# `paths-ignore:`, each reporting the same branch-protection-required check
# names. The companion's own comment claimed "for any PR exactly one of the two
# workflows runs". That is false: `paths-ignore` fires when ANY changed file
# falls outside the list, so a PR touching both a gated and an ungated path ran
# BOTH workflows and two check runs answered to one required context name --
# which one branch protection resolved came down to which finished last. It was
# also two copies of one path list, kept in step by a comment; drift in which a
# path matched NEITHER list would have re-created the original "Expected --
# waiting for status" deadlock the companion was added to fix.
#
# One list, one workflow, one check run per context. The gate job always runs
# and always reports; this script decides whether it does the expensive work or
# passes immediately with a line saying why. "Matches no list" is impossible by
# construction here, because there is only one list and the two outcomes are
# its boolean complement.
#
# Usage:
#   scripts/gate-paths-changed.sh [--files-from <file>] < patterns
#
# Patterns come in on stdin, one per line; `#` comments and blank lines are
# ignored. Two forms only, both validated (anything else is a hard error, so a
# pattern that would silently match nothing fails the gate loudly instead):
#   path/to/file      exact path
#   path/to/dir/**    that directory and everything under it
#
# Prints `changed=true` or `changed=false` on stdout, in $GITHUB_OUTPUT form.
# Everything human-readable goes to stderr.
#
# Changed files come from the GitHub API (`repos/{repo}/pulls/{n}/files`),
# which diffs against the merge base -- the same set GitHub's own path filters
# and the PR "Files changed" tab use. --files-from reads a newline-separated
# list from a file instead, for tests.
#
# FAIL-SAFE, deliberately: if the file list cannot be established or looks
# truncated, this reports `changed=true` so the gate runs for real. The failure
# mode of a gate is never "quietly decide it does not apply".
set -euo pipefail

FILES_FROM=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --files-from) FILES_FROM="${2:-}"; shift 2 ;;
    -h|--help) sed -n '2,40p' "$0"; exit 0 ;;
    *) echo "gate-paths-changed.sh: unknown argument: $1" >&2; exit 2 ;;
  esac
done

note() { printf '%s\n' "$*" >&2; }

# ---- patterns ---------------------------------------------------------------
patterns=()
while IFS= read -r line || [[ -n "$line" ]]; do
  line="${line%%#*}"
  # trim
  line="${line#"${line%%[![:space:]]*}"}"
  line="${line%"${line##*[![:space:]]}"}"
  [[ -z "$line" ]] && continue
  patterns+=("$line")
done

if [[ ${#patterns[@]} -eq 0 ]]; then
  echo "gate-paths-changed.sh: no patterns given on stdin" >&2
  exit 2
fi

for p in "${patterns[@]}"; do
  base="$p"
  # Quoted so bash compares the literal three characters "/**"; unquoted, `*/**`
  # is a glob that matches ANY path containing a slash, which quietly turned
  # every exact-path pattern into a prefix match.
  [[ "$p" == *"/**" ]] && base="${p%"/**"}"
  case "$base" in
    *[\*\?\[\]]*)
      echo "gate-paths-changed.sh: unsupported pattern '$p'." >&2
      echo "  Only an exact path or a 'dir/**' prefix is allowed -- no other glob syntax." >&2
      echo "  A pattern this script cannot honour must fail the gate, not silently match nothing." >&2
      exit 2 ;;
  esac
  case "$base" in
    /*|*/) echo "gate-paths-changed.sh: pattern '$p' must be repo-relative and must not end in '/'" >&2; exit 2 ;;
  esac
done

# ---- changed files ----------------------------------------------------------
emit() { printf 'changed=%s\n' "$1"; }

changed_files=""
if [[ -n "$FILES_FROM" ]]; then
  changed_files="$(cat "$FILES_FROM")"
else
  repo="${GH_REPO:-${GITHUB_REPOSITORY:-}}"
  pr="${PR_NUMBER:-}"
  if [[ -z "$repo" || -z "$pr" ]]; then
    note "gate-paths-changed.sh: no PR context (GH_REPO='$repo', PR_NUMBER='$pr') -- running the gate for real."
    emit true; exit 0
  fi
  if ! changed_files="$(gh api --paginate "repos/$repo/pulls/$pr/files" \
        --jq '.[] | .filename, (.previous_filename // empty)' 2>/dev/null)"; then
    note "gate-paths-changed.sh: could not list PR #$pr's files -- running the gate for real."
    emit true; exit 0
  fi
  # The files endpoint is capped (3000 files). Cross-check against the PR's own
  # count so a truncated list can never be read as "nothing relevant changed".
  if declared="$(gh api "repos/$repo/pulls/$pr" --jq '.changed_files' 2>/dev/null)"; then
    listed="$(printf '%s\n' "$changed_files" | grep -c . || true)"
    if [[ -n "$declared" && "$listed" -lt "$declared" ]]; then
      note "gate-paths-changed.sh: PR #$pr declares $declared changed files but the API listed $listed -- truncated; running the gate for real."
      emit true; exit 0
    fi
  fi
fi

# ---- match ------------------------------------------------------------------
matched=()
while IFS= read -r f; do
  [[ -z "$f" ]] && continue
  for p in "${patterns[@]}"; do
    if [[ "$p" == *"/**" ]]; then
      prefix="${p%"**"}"          # keeps the trailing slash
      [[ "$f" == "$prefix"* ]] && { matched+=("$f"); break; }
    else
      [[ "$f" == "$p" ]] && { matched+=("$f"); break; }
    fi
  done
done <<< "$changed_files"

total="$(printf '%s\n' "$changed_files" | grep -c . || true)"
if [[ ${#matched[@]} -gt 0 ]]; then
  note "$total changed file(s); ${#matched[@]} match this gate's paths:"
  printf '  %s\n' "${matched[@]}" >&2
  emit true
else
  note "$total changed file(s); none match this gate's paths. Gate not applicable."
  emit false
fi
