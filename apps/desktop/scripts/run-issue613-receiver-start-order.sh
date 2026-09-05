#!/usr/bin/env bash
set -euo pipefail

# Bounded #613 experiment. Each source matrix changes one factor: whether the
# production receiver connects before publication or at publisher age 8s.
# Synthetic isolation and real capture confirmation never share a verdict.

MATRIX="${PETAL_613_MATRIX:-both}"
SELF_TEST=0
while (($#)); do
  case "$1" in
    --matrix) MATRIX="${2:-}"; shift 2 ;;
    --self-test) SELF_TEST=1; shift ;;
    *) echo "usage: $0 [--matrix synthetic|real|both] [--self-test]" >&2; exit 2 ;;
  esac
done
case "$MATRIX" in synthetic|real|both) ;; *) echo "invalid matrix: $MATRIX" >&2; exit 2 ;; esac

combined_conclusion() {
  if [[ "$1" == SUPPORTED && "$2" == SUPPORTED ]]; then
    echo CONFIRMED_BY_BOTH_DIAGNOSTIC_MATRICES
  else
    echo NO_PRODUCT_CONCLUSION
  fi
}

if ((SELF_TEST)); then
  [[ "$(combined_conclusion SUPPORTED SUPPORTED)" == CONFIRMED_BY_BOTH_DIAGNOSTIC_MATRICES ]]
  [[ "$(combined_conclusion SUPPORTED FALSIFIED)" == NO_PRODUCT_CONCLUSION ]]
  [[ "$(combined_conclusion NOT_RUN SUPPORTED)" == NO_PRODUCT_CONCLUSION ]]
  echo "SELF_TEST_PASS two-matrix conclusion gate"
  exit 0
fi

DESKTOP_DIR="$(cd "$(dirname "$0")/.." && pwd)"
TAURI_DIR="$DESKTOP_DIR/src-tauri"
PUBLISHER="$TAURI_DIR/target/debug/examples/publish_probe"
SUBSCRIBER="$TAURI_DIR/target/debug/examples/subscribe_probe"
TARGET_SCRIPT="$DESKTOP_DIR/scripts/latency-target-window.swift"
PORT="${PETAL_613_PORT:-7893}"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUT="${PETAL_613_OUTPUT_DIR:-/private/tmp/petal-613-receiver-start-order-$STAMP}"
mkdir -p "$OUT"

epoch_us() {
  perl -MTime::HiRes=time -e 'printf "%.0f\n", time() * 1000000'
}

LEASE="$OUT/owned-process-lease.tsv"
printf 'event\trole\tpid\tepoch_us\n' >"$LEASE"

for executable in "$PUBLISHER" "$SUBSCRIBER"; do
  if [[ ! -x "$executable" ]]; then
    echo "BLOCKED: build examples first: cargo build --locked --example publish_probe --example subscribe_probe" >&2
    exit 2
  fi
done
for command in livekit-server jq nc perl; do
  command -v "$command" >/dev/null || { echo "BLOCKED: missing $command" >&2; exit 2; }
done
if [[ "$MATRIX" != synthetic ]]; then
  command -v swift >/dev/null || { echo "BLOCKED: missing swift" >&2; exit 2; }
fi
if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "BLOCKED: port $PORT is already in use; no process was touched" >&2
  exit 2
fi

OWNED_PIDS=()
lease_owned_pid() {
  local role="$1" pid="$2"
  printf 'STARTED\t%s\t%s\t%s\n' "$role" "$pid" "$(epoch_us)" >>"$LEASE"
}
cleanup() {
  local pid
  for pid in "${OWNED_PIDS[@]:-}"; do
    if kill -0 "$pid" 2>/dev/null; then
      printf 'TERMINATING\tcleanup\t%s\t%s\n' "$pid" "$(epoch_us)" >>"$LEASE"
      kill "$pid" 2>/dev/null || true
    fi
  done
  for pid in "${OWNED_PIDS[@]:-}"; do
    wait "$pid" 2>/dev/null || true
    printf 'CLEANED\tcleanup\t%s\t%s\n' "$pid" "$(epoch_us)" >>"$LEASE"
  done
}
trap cleanup EXIT INT TERM

forget_owned_pid() {
  local completed="$1" role="${2:-arm}" pid
  local -a remaining=()
  for pid in "${OWNED_PIDS[@]:-}"; do
    [[ "$pid" == "$completed" ]] || remaining+=("$pid")
  done
  OWNED_PIDS=("${remaining[@]}")
  printf 'COMPLETED\t%s\t%s\t%s\n' "$role" "$completed" "$(epoch_us)" >>"$LEASE"
}

export LIVEKIT_URL="ws://127.0.0.1:$PORT"
export LIVEKIT_API_KEY=devkey
export LIVEKIT_API_SECRET=secret

livekit-server --dev --bind 127.0.0.1 --port "$PORT" >"$OUT/livekit.log" 2>&1 &
SFU_PID=$!
OWNED_PIDS+=("$SFU_PID")
lease_owned_pid sfu "$SFU_PID"
for _ in {1..100}; do
  nc -z 127.0.0.1 "$PORT" >/dev/null 2>&1 && break
  kill -0 "$SFU_PID" 2>/dev/null || { echo "BLOCKED: local SFU exited" >&2; exit 2; }
  sleep 0.1
done
nc -z 127.0.0.1 "$PORT" >/dev/null 2>&1 || { echo "BLOCKED: local SFU did not listen" >&2; exit 2; }

WINDOW_ID=0
if [[ "$MATRIX" != synthetic ]]; then
  swift "$TARGET_SCRIPT" --width 1600 --height 900 --fps 30 --seconds 1200 >"$OUT/target.log" 2>&1 &
  TARGET_PID=$!
  OWNED_PIDS+=("$TARGET_PID")
  lease_owned_pid target "$TARGET_PID"
  WINDOW_ID=""
  for _ in {1..100}; do
    WINDOW_ID="$(sed -n 's/^WINDOW_ID //p' "$OUT/target.log" | tail -1)"
    [[ -n "$WINDOW_ID" ]] && break
    kill -0 "$TARGET_PID" 2>/dev/null || { echo "BLOCKED: target exited" >&2; exit 2; }
    sleep 0.1
  done
  [[ -n "$WINDOW_ID" ]] || { echo "BLOCKED: target produced no WINDOW_ID" >&2; exit 2; }
fi

wait_for_log() {
  local file="$1" pattern="$2" pid="$3"
  for _ in {1..150}; do
    grep -q "$pattern" "$file" 2>/dev/null && return 0
    kill -0 "$pid" 2>/dev/null || return 1
    sleep 0.1
  done
  return 1
}

wait_until_epoch_us() {
  local target_us="$1" now
  while true; do
    now="$(epoch_us)"
    (( now >= target_us )) && return 0
    sleep 0.025
  done
}

run_arm() {
  local source="$1" label="$2" order="$3" pin_ms="$4" matrix_dir="$5" results="$6"
  local room="petal-613-${STAMP}-${source}-${label}"
  local sub_log="$matrix_dir/${label}-subscriber.log"
  local pub_log="$matrix_dir/${label}-publisher.log"
  local dump="$matrix_dir/${label}.csv"
  local boundary="$matrix_dir/${label}-measurement-window.txt"
  local -a publisher_args
  if [[ "$source" == synthetic ]]; then
    publisher_args=("$PUBLISHER" 0 "$room" --source synthetic --seconds 30 --fps 30 --width 1600 --height 900 --measurement-window-file "$boundary")
  else
    publisher_args=("$PUBLISHER" "$WINDOW_ID" "$room" --source real --seconds 30 --expected-capture-width 1600 --expected-capture-height 900 --measurement-window-file "$boundary")
  fi

  local -a sub_env=(env -u PETAL_PLAYOUT_DELAY_MS RUST_LOG=info PETAL_PROBE_DUMP="$dump")
  if [[ "$pin_ms" != unset ]]; then
    sub_env=(env RUST_LOG=info PETAL_PROBE_DUMP="$dump" PETAL_PLAYOUT_DELAY_MS="$pin_ms")
  fi

  if [[ "$order" == early ]]; then
    "${sub_env[@]}" "$SUBSCRIBER" "$room" --measurement-window-file "$boundary" --first-frame-timeout-ms 15000 >"$sub_log" 2>&1 &
    SUB_PID=$!; OWNED_PIDS+=("$SUB_PID")
    lease_owned_pid "$label-subscriber" "$SUB_PID"
    wait_for_log "$sub_log" PROBE_SUBSCRIBER_CONNECTED "$SUB_PID" || { echo "INVALID $label: subscriber did not connect" >&2; return 3; }
    "${publisher_args[@]}" >"$pub_log" 2>&1 &
    PUB_PID=$!; OWNED_PIDS+=("$PUB_PID")
    lease_owned_pid "$label-publisher" "$PUB_PID"
  else
    "${publisher_args[@]}" >"$pub_log" 2>&1 &
    PUB_PID=$!; OWNED_PIDS+=("$PUB_PID")
    lease_owned_pid "$label-publisher" "$PUB_PID"
  fi

  if [[ "$source" == real ]]; then
    wait_for_log "$pub_log" '^CAPTURE_RASTER_VERIFIED 1600x900$' "$PUB_PID" || {
      echo "INVALID $label: real capture did not verify exact 1600x900 raster before publish" >&2
      return 3
    }
  fi
  wait_for_log "$pub_log" '^Published\.' "$PUB_PID" || { echo "INVALID $label: publisher did not publish" >&2; return 3; }
  local published_observed_us measurement_start_us measurement_end_us boundary_tmp
  published_observed_us="$(epoch_us)"
  measurement_start_us=$((published_observed_us + 16000000))
  measurement_end_us=$((published_observed_us + 28000000))
  boundary_tmp="$boundary.tmp"
  printf '%s %s\n' "$measurement_start_us" "$measurement_end_us" >"$boundary_tmp"
  mv "$boundary_tmp" "$boundary"

  if [[ "$order" == late ]]; then
    wait_until_epoch_us $((published_observed_us + 8000000))
    "${sub_env[@]}" "$SUBSCRIBER" "$room" --measurement-window-file "$boundary" --first-frame-timeout-ms 10000 >"$sub_log" 2>&1 &
    SUB_PID=$!; OWNED_PIDS+=("$SUB_PID")
    lease_owned_pid "$label-subscriber" "$SUB_PID"
  fi

  wait "$SUB_PID" || { echo "INVALID $label: subscriber failed" >&2; return 3; }
  forget_owned_pid "$SUB_PID" "$label-subscriber"
  wait "$PUB_PID" || { echo "INVALID $label: publisher failed" >&2; return 3; }
  forget_owned_pid "$PUB_PID" "$label-publisher"

  local json publisher_json
  json="$(sed -n 's/^PROBE_RESULT_JSON //p' "$sub_log" | tail -1)"
  [[ -n "$json" ]] || { echo "INVALID $label: missing result JSON" >&2; return 3; }
  publisher_json="$(sed -n 's/^PUBLISHER_WINDOW_JSON //p' "$pub_log" | tail -1)"
  [[ -n "$publisher_json" ]] || { echo "INVALID $label: missing aligned publisher evidence" >&2; return 3; }
  json="$(jq -c --argjson publisher "$publisher_json" '. + $publisher' <<<"$json")"
  if ! jq -e --arg source "$source" '
    .source == $source and
    .measurement_seconds >= 11.5 and .measurement_seconds <= 12.5 and
    .publisher_measurement_seconds >= 11.5 and .publisher_measurement_seconds <= 12.5 and
    .measurement_start_epoch_us == .publisher_scheduled_measurement_start_epoch_us and
    .measurement_end_epoch_us == .publisher_scheduled_measurement_end_epoch_us and
    .measurement_n >= 300 and .missing_timestamps == 0 and
    .decoded_fps >= 25 and .decoded_fps <= 35 and
    .publisher_pushed_fps >= 25 and .publisher_pushed_fps <= 35 and
    .end_to_end_publisher_frame_gaps == 0 and .receiver_frames_dropped == 0 and
    .capture_overwrite_ratio <= 0.01 and
    .publisher_quality_limitation_valid == true and
    (.outbound_video_encoding_snapshots.start | length) > 0 and
    (.outbound_video_encoding_snapshots.end | length) > 0 and
    ([.outbound_video_encoding_snapshots.start[], .outbound_video_encoding_snapshots.end[]] |
      all(.[];
        (.quality_limitation != "") and
        (.quality_limitation != "Cpu") and
        (.quality_limitation != "Bandwidth"))) and
    (.jitter_target_ms != null) and
    (.capture_callback_to_decoded_callback_p50_ms != null)
  ' <<<"$json" >/dev/null; then
    echo "INVALID $label: preregistered validity boundary failed: $json" >&2
    return 3
  fi
  if [[ "$pin_ms" != unset ]]; then
    grep -q 'receiver setter returned Ok(true)' "$sub_log" || { echo "INVALID $label: playout setter not confirmed" >&2; return 3; }
    ! grep -q PLAYOUT_DELAY_NOT_APPLIED "$sub_log" || { echo "INVALID $label: playout setter failed" >&2; return 3; }
  fi

  jq -c --arg matrix "$source" --arg label "$label" --arg order "$order" --arg pin "$pin_ms" \
    '. + {matrix:$matrix, label:$label, order:$order, pin:$pin}' <<<"$json" >>"$results"
  echo "VALID $source/$label: $(tail -1 "$results")"
}

write_matrix_verdict() {
  local source="$1" results="$2" verdict="$3"
  jq -s --arg matrix "$source" '
  def median4: sort | (.[1] + .[2]) / 2;
  def value($label; $field): first(.[] | select(.label == $label) | .[$field]);
  . as $rows |
  [range(1;5) as $rep |
    {rep:$rep,
     target_delta:(value("r\($rep)-early"; "jitter_target_ms") - value("r\($rep)-late"; "jitter_target_ms")),
     lower_bound_delta:(value("r\($rep)-early"; "capture_callback_to_decoded_callback_p50_ms") - value("r\($rep)-late"; "capture_callback_to_decoded_callback_p50_ms"))}
  ] as $pairs |
  (value("control-early-400"; "jitter_target_ms") - value("r4-early"; "jitter_target_ms")) as $control_target |
  (value("control-early-400"; "capture_callback_to_decoded_callback_p50_ms") - value("r4-early"; "capture_callback_to_decoded_callback_p50_ms")) as $control_lower_bound |
  {
    matrix:$matrix,
    pairs:$pairs,
    median_target_delta:([$pairs[].target_delta] | median4),
    positive_control:{target_delta:$control_target,lower_bound_delta:$control_lower_bound,passed:($control_target >= 100 and $control_lower_bound >= 100)},
    hypothesis:(if ($control_target < 100 or $control_lower_bound < 100) then "INVALID_CONTROL"
      elif (all($pairs[]; .target_delta > 0 and .lower_bound_delta > 0) and ([$pairs[].target_delta] | median4) >= 32)
      then "SUPPORTED" else "FALSIFIED" end)
  }
' "$results" | tee "$verdict"
}

run_matrix() {
  local source="$1" matrix_dir="$OUT/$1" results verdict
  mkdir -p "$matrix_dir"
  results="$matrix_dir/results.jsonl"
  verdict="$matrix_dir/verdict.json"
  : >"$results"

  # Identical four-pair rotation and positive-control gate for each source.
  run_arm "$source" r1-early early unset "$matrix_dir" "$results" || return $?
  run_arm "$source" r1-late late unset "$matrix_dir" "$results" || return $?
  run_arm "$source" r2-late late unset "$matrix_dir" "$results" || return $?
  run_arm "$source" r2-early early unset "$matrix_dir" "$results" || return $?
  run_arm "$source" r3-early early unset "$matrix_dir" "$results" || return $?
  run_arm "$source" r3-late late unset "$matrix_dir" "$results" || return $?
  run_arm "$source" r4-late late unset "$matrix_dir" "$results" || return $?
  run_arm "$source" r4-early early unset "$matrix_dir" "$results" || return $?
  run_arm "$source" control-early-400 early 400 "$matrix_dir" "$results" || return $?

  write_matrix_verdict "$source" "$results" "$verdict"
  [[ "$(jq -r .hypothesis "$verdict")" != INVALID_CONTROL ]] || return 3
}

write_combined_verdict() {
  local synthetic="$1" real="$2" conclusion
  conclusion="$(combined_conclusion "$synthetic" "$real")"
  jq -n --arg synthetic "$synthetic" --arg real "$real" --arg conclusion "$conclusion" '{
    synthetic_matrix:$synthetic,
    real_capture_matrix:$real,
    product_conclusion:$conclusion,
    note:"Diagnostic evidence only; never a product patch by itself."
  }' | tee "$OUT/combined-verdict.json"
}

case "$MATRIX" in
  synthetic)
    run_matrix synthetic || { status=$?; write_combined_verdict INVALID NOT_RUN; exit "$status"; }
    write_combined_verdict "$(jq -r .hypothesis "$OUT/synthetic/verdict.json")" NOT_RUN
    ;;
  real)
    run_matrix real || { status=$?; write_combined_verdict NOT_RUN INVALID; exit "$status"; }
    write_combined_verdict NOT_RUN "$(jq -r .hypothesis "$OUT/real/verdict.json")"
    ;;
  both)
    run_matrix synthetic || { status=$?; write_combined_verdict INVALID NOT_RUN; exit "$status"; }
    synthetic_verdict="$(jq -r .hypothesis "$OUT/synthetic/verdict.json")"
    run_matrix real || { status=$?; write_combined_verdict "$synthetic_verdict" INVALID; exit "$status"; }
    write_combined_verdict "$synthetic_verdict" "$(jq -r .hypothesis "$OUT/real/verdict.json")"
    ;;
esac

echo "EVIDENCE_DIR $OUT"
echo "Nothing this script started is intended to survive cleanup."
