#!/usr/bin/env bash
#
# Run a command with source-state invalidation. Default/raw mode is explicitly
# unverified. Only --require-clean materializes canonical HEAD in an isolated
# checkout and may supply trusted provenance to a release/QA build.
set -euo pipefail

require_clean=0
if [[ "${1:-}" == "--require-clean" ]]; then
  require_clean=1
  shift
fi

if [[ "$#" -eq 0 ]]; then
  echo "usage: $0 [--require-clean] <command> [args...]" >&2
  exit 64
fi

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  echo "source provenance: current directory is not in a Git worktree" >&2
  exit 2
}
repo_root="$(cd "$repo_root" && pwd -P)"
caller_cwd="$(pwd -P)"
case "$caller_cwd" in
  "$repo_root") relative_cwd="" ;;
  "$repo_root"/*) relative_cwd="${caller_cwd#"$repo_root"/}" ;;
  *)
    echo "source provenance: current directory is outside the resolved worktree" >&2
    exit 2
    ;;
esac

canonical_head() {
  local root="$1" sha
  sha="$(git -C "$root" rev-parse --verify HEAD 2>/dev/null)" || return 1
  [[ "$sha" =~ ^[0-9a-f]{40}$ ]] || return 1
  printf '%s' "$sha"
}

# Hash canonical HEAD, full porcelain state, and the contents/modes of every
# tracked or nonignored untracked input. Paths are NUL-delimited throughout.
source_fingerprint() {
  local root="$1" head_sha="$2" manifest path mode digest fingerprint
  manifest="$(mktemp "${TMPDIR:-/tmp}/petal-source-manifest.XXXXXX")"
  if ! {
    printf 'HEAD\0%s\0STATUS\0' "$head_sha"
    GIT_OPTIONAL_LOCKS=0 git -C "$root" status \
      --porcelain=v1 -z --untracked-files=all
    printf '\0FILES\0'
    while IFS= read -r -d '' path; do
      if [[ ! -e "$root/$path" && ! -L "$root/$path" ]]; then
        mode="deleted"
        digest="deleted"
      elif [[ -L "$root/$path" ]]; then
        mode="$(stat -f '%Lp' "$root/$path")"
        digest="$(readlink "$root/$path" | shasum -a 256 | awk '{print $1}')"
      elif [[ -f "$root/$path" ]]; then
        mode="$(stat -f '%Lp' "$root/$path")"
        digest="$(shasum -a 256 -- "$root/$path" | awk '{print $1}')"
      else
        mode="$(stat -f '%Lp' "$root/$path")"
        digest="unsupported"
      fi
      printf '%s\0%s\0%s\0' "$path" "$mode" "$digest"
    done < <(
      git -C "$root" ls-files -z --cached --others --exclude-standard
    )
  } >"$manifest"; then
    rm -f "$manifest"
    return 1
  fi
  if ! fingerprint="$(shasum -a 256 "$manifest" | awk '{print $1}')"; then
    rm -f "$manifest"
    return 1
  fi
  rm -f "$manifest"
  printf '%s' "$fingerprint"
}

head_sha="$(canonical_head "$repo_root")" || {
  echo "source provenance: cannot resolve canonical HEAD" >&2
  exit 2
}
caller_state="$(source_fingerprint "$repo_root" "$head_sha")"

# Raw/local-CI mode carries a complete state only for Cargo invalidation. It
# never claims a trusted SHA and deliberately runs in the caller worktree.
if [[ "$require_clean" -eq 0 ]]; then
  export PETAL_OFFICIAL_SOURCE_SHA_FULL="unverified"
  export PETAL_OFFICIAL_SOURCE_STATE="$caller_state"
  export PETAL_SOURCE_PROVENANCE_WRAPPED="$caller_state"
  exec "$@"
fi

caller_status="$(mktemp "${TMPDIR:-/tmp}/petal-source-status.XXXXXX")"
isolation_root="$(mktemp -d "${TMPDIR:-/tmp}/petal-trusted-source.XXXXXX")"
cleanup() {
  rm -f "$caller_status"
  rm -rf "$isolation_root"
}
trap cleanup EXIT

GIT_OPTIONAL_LOCKS=0 git -C "$repo_root" status \
  --porcelain=v1 --untracked-files=all >"$caller_status"
if [[ -s "$caller_status" ]]; then
  echo "source provenance: refusing official release build from a non-clean worktree" >&2
  exit 3
fi

materialized_root="$isolation_root/repo"
git clone --quiet --no-checkout --shared "$repo_root" "$materialized_root"
git -C "$materialized_root" checkout --quiet --detach "$head_sha"
materialized_head="$(canonical_head "$materialized_root")" || {
  echo "source provenance: isolated checkout has no canonical HEAD" >&2
  exit 4
}
[[ "$materialized_head" == "$head_sha" ]] || {
  echo "source provenance: isolated checkout resolved a different HEAD" >&2
  exit 4
}
materialized_state="$(source_fingerprint "$materialized_root" "$materialized_head")"
[[ "$materialized_state" == "$caller_state" ]] || {
  echo "source provenance: isolated checkout does not match canonical caller HEAD" >&2
  exit 4
}

isolated_cwd="$materialized_root"
if [[ -n "$relative_cwd" ]]; then
  isolated_cwd="$materialized_root/$relative_cwd"
fi
[[ -d "$isolated_cwd" ]] || {
  echo "source provenance: isolated command directory is missing" >&2
  exit 4
}

export PETAL_OFFICIAL_SOURCE_SHA_FULL="$head_sha"
export PETAL_OFFICIAL_SOURCE_STATE="$materialized_state"
export PETAL_SOURCE_PROVENANCE_WRAPPED="$materialized_state"
export PETAL_PROVENANCE_OUTPUT_ROOT="$repo_root"

set +e
(
  cd "$isolated_cwd"
  "$@"
)
command_status=$?
set -e

final_materialized_head="$(canonical_head "$materialized_root")" || {
  echo "source provenance: cannot re-resolve isolated HEAD after command" >&2
  exit 4
}
final_materialized_state="$(
  source_fingerprint "$materialized_root" "$final_materialized_head"
)"
final_caller_head="$(canonical_head "$repo_root")" || {
  echo "source provenance: cannot re-resolve caller HEAD after command" >&2
  exit 4
}
final_caller_state="$(source_fingerprint "$repo_root" "$final_caller_head")"

if [[ "$final_materialized_head" != "$head_sha" ]] ||
   [[ "$final_materialized_state" != "$materialized_state" ]]; then
  echo "source provenance: isolated source changed while command was running; refusing success" >&2
  exit 4
fi
if [[ "$final_caller_head" != "$head_sha" ]] ||
   [[ "$final_caller_state" != "$caller_state" ]]; then
  echo "source provenance: caller source changed while command was running; refusing success" >&2
  exit 4
fi

exit "$command_status"
