#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Validate, aggregate, and compare a judged Internal_Bench_hard run.

This does not run the LLM judges. It expects chunk_*.json files to already
exist in the judge directory.

Usage:
  scripts/finalize-ibh-judged-run.sh --run-root ROOT [--run-id ID]
                                            [--judge-dir DIR]
                                            [--reference-aggregate FILE]
                                            [--out FILE]
                                            [--expected-total N]
                                            [--current-expected-total N]
                                            [--reference-expected-total N]
                                            [--task-id ID ...]
USAGE
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

REFERENCE_AGGREGATE="${REFERENCE_AGGREGATE:-/home/exedev/eval-runs/ibh-purecodex-175254-rejudge-jsonl-20260613/judge_aggregate.json}"
EXPECTED_TOTAL="${EXPECTED_TOTAL:-106}"
CURRENT_EXPECTED_TOTAL=""
REFERENCE_EXPECTED_TOTAL=""
RUN_ROOT=""
RUN_ID=""
JUDGE_DIR=""
OUT=""
TASK_IDS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --run-root)
      RUN_ROOT="${2:?--run-root requires a value}"
      shift 2
      ;;
    --run-id)
      RUN_ID="${2:?--run-id requires a value}"
      shift 2
      ;;
    --judge-dir)
      JUDGE_DIR="${2:?--judge-dir requires a value}"
      shift 2
      ;;
    --reference-aggregate)
      REFERENCE_AGGREGATE="${2:?--reference-aggregate requires a value}"
      shift 2
      ;;
    --out)
      OUT="${2:?--out requires a value}"
      shift 2
      ;;
    --expected-total)
      EXPECTED_TOTAL="${2:?--expected-total requires a value}"
      shift 2
      ;;
    --current-expected-total)
      CURRENT_EXPECTED_TOTAL="${2:?--current-expected-total requires a value}"
      shift 2
      ;;
    --reference-expected-total)
      REFERENCE_EXPECTED_TOTAL="${2:?--reference-expected-total requires a value}"
      shift 2
      ;;
    --task-id)
      TASK_IDS+=("${2:?--task-id requires a value}")
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ -z "$RUN_ROOT" ]]; then
  echo "--run-root is required" >&2
  usage >&2
  exit 2
fi

if [[ ! -d "$RUN_ROOT" ]]; then
  echo "run root not found: $RUN_ROOT" >&2
  exit 1
fi

RUN_ROOT="$(cd "$RUN_ROOT" && pwd)"
RUN_ID="${RUN_ID:-$(basename "$RUN_ROOT")}"
JUDGE_DIR="${JUDGE_DIR:-$RUN_ROOT/judge}"
OUT="${OUT:-$RUN_ROOT/current-vs-raw-judged-delta.md}"
CURRENT_EXPECTED_TOTAL="${CURRENT_EXPECTED_TOTAL:-$EXPECTED_TOTAL}"
REFERENCE_EXPECTED_TOTAL="${REFERENCE_EXPECTED_TOTAL:-$EXPECTED_TOTAL}"

if [[ ! -d "$JUDGE_DIR" ]]; then
  echo "judge dir not found: $JUDGE_DIR" >&2
  exit 1
fi

if [[ ! -f "$REFERENCE_AGGREGATE" ]]; then
  echo "reference aggregate not found: $REFERENCE_AGGREGATE" >&2
  exit 1
fi

"$REPO_ROOT/scripts/aggregate-ibh-judgments.py" "$JUDGE_DIR" \
  --run-root "$RUN_ROOT" \
  --run-id "$RUN_ID" \
  --expected-total "$CURRENT_EXPECTED_TOTAL"

compare_args=(
  "$REPO_ROOT/scripts/compare-judged-runs.py"
  --current-aggregate "$JUDGE_DIR/judge_aggregate.json"
  --reference-aggregate "$REFERENCE_AGGREGATE"
  --current-label "$RUN_ID"
  --reference-label raw-codex-browser-harness-96
  --current-expected-total "$CURRENT_EXPECTED_TOTAL"
  --reference-expected-total "$REFERENCE_EXPECTED_TOTAL"
  --out "$OUT"
)

if [[ "${#TASK_IDS[@]}" -gt 0 ]]; then
  for task_id in "${TASK_IDS[@]}"; do
    compare_args+=(--task-id "$task_id")
  done
else
  compare_args+=(--expected-total "$EXPECTED_TOTAL")
fi

"${compare_args[@]}"

jq -r '
  "score=\(.passed)/\(.expected_total) failed=\(.failed) failed_ids=\(.failed_ids | join(","))"
' "$JUDGE_DIR/judge_aggregate.json"
echo "comparison=$OUT"
