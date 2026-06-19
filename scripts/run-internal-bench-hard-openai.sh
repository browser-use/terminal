#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Run Internal_Bench_hard through the product simple/raw browser-harness path.

Required environment:
  OPENAI_API_KEY or LLM_BROWSER_OPENAI_API_KEY
    or stored auth from: browser-use-terminal auth login openai
  BROWSER_USE_API_KEY
    or stored auth from: browser-use-terminal auth login browser-use-cloud

Optional environment:
  ENV_FILE=.env
  DATASET=/home/exedev/datasets/Internal_Bench_hard.json
  BROWSER_HARNESS_SRC=/home/exedev/repos/browser-harness/src
  MODEL=gpt-5.5
  CONCURRENCY=25
  TASK_TIMEOUT_SECONDS=2700
  TASK_TOKEN_CAP=30000000
  TASK_SAFETY_POLL_SECONDS=10
  ULIMIT_NOFILE=65535
  OUT_BASE=/home/exedev/eval-runs
  JUDGE_AFTER_RUN=0
  JUDGE_MODEL=sonnet
  JUDGE_CONCURRENCY=5
  BROWSER_USE_TERMINAL_BIN=target/debug/browser-use-terminal
  REFERENCE_AGGREGATE=/home/exedev/eval-runs/ibh-purecodex-175254-rejudge-jsonl-20260613/judge_aggregate.json

Usage:
  scripts/run-internal-bench-hard-openai.sh [--run-id ID] [--root DIR]
                                           [--concurrency N] [--skip-build]
                                           [--task-id ID ...]
                                           [--dry-run] [--judge]
USAGE
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

ENV_FILE="${ENV_FILE:-$REPO_ROOT/.env}"
if [[ -f "$ENV_FILE" ]]; then
  set -a
  # shellcheck disable=SC1090
  . "$ENV_FILE"
  set +a
fi

DATASET="${DATASET:-/home/exedev/datasets/Internal_Bench_hard.json}"
BROWSER_HARNESS_SRC="${BROWSER_HARNESS_SRC:-/home/exedev/repos/browser-harness/src}"
MODEL="${MODEL:-gpt-5.5}"
CONCURRENCY="${CONCURRENCY:-25}"
MAX_TURNS="${MAX_TURNS:-10000}"
PYTHON_TIMEOUT_SECONDS="${PYTHON_TIMEOUT_SECONDS:-180}"
TASK_TIMEOUT_SECONDS="${TASK_TIMEOUT_SECONDS:-2700}"
TASK_TOKEN_CAP="${TASK_TOKEN_CAP:-30000000}"
TASK_SAFETY_POLL_SECONDS="${TASK_SAFETY_POLL_SECONDS:-10}"
ULIMIT_NOFILE="${ULIMIT_NOFILE:-65535}"
OUT_BASE="${OUT_BASE:-/home/exedev/eval-runs}"
HEALTH_AFTER_SECONDS="${HEALTH_AFTER_SECONDS:-30}"
JUDGE_AFTER_RUN="${JUDGE_AFTER_RUN:-0}"
JUDGE_MODEL="${JUDGE_MODEL:-sonnet}"
JUDGE_CONCURRENCY="${JUDGE_CONCURRENCY:-5}"
JUDGE_CLAUDE_BIN="${JUDGE_CLAUDE_BIN:-claude}"
REFERENCE_AGGREGATE="${REFERENCE_AGGREGATE:-/home/exedev/eval-runs/ibh-purecodex-175254-rejudge-jsonl-20260613/judge_aggregate.json}"
JUDGE_OVERWRITE=0
TASK_IDS=()
RUN_ID=""
ROOT=""
SKIP_BUILD=0
DRY_RUN=0
BIN="${BROWSER_USE_TERMINAL_BIN:-$REPO_ROOT/target/debug/browser-use-terminal}"
if [[ "$BIN" != /* ]]; then
  BIN="$REPO_ROOT/$BIN"
fi
AUTH_PREFLIGHT_AFTER_BUILD=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --run-id)
      RUN_ID="${2:?--run-id requires a value}"
      shift 2
      ;;
    --root)
      ROOT="${2:?--root requires a value}"
      shift 2
      ;;
    --concurrency)
      CONCURRENCY="${2:?--concurrency requires a value}"
      shift 2
      ;;
    --task-id)
      TASK_IDS+=("${2:?--task-id requires a value}")
      shift 2
      ;;
    --skip-build)
      SKIP_BUILD=1
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --judge)
      JUDGE_AFTER_RUN=1
      shift
      ;;
    --judge-model)
      JUDGE_MODEL="${2:?--judge-model requires a value}"
      shift 2
      ;;
    --judge-concurrency)
      JUDGE_CONCURRENCY="${2:?--judge-concurrency requires a value}"
      shift 2
      ;;
    --judge-claude-bin)
      JUDGE_CLAUDE_BIN="${2:?--judge-claude-bin requires a value}"
      shift 2
      ;;
    --overwrite-judge)
      JUDGE_OVERWRITE=1
      shift
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

require_file() {
  local path="$1"
  local label="$2"
  if [[ ! -f "$path" ]]; then
    echo "$label not found: $path" >&2
    exit 1
  fi
}

require_dir() {
  local path="$1"
  local label="$2"
  if [[ ! -d "$path" ]]; then
    echo "$label not found: $path" >&2
    exit 1
  fi
}

require_file "$DATASET" "Internal_Bench_hard dataset"
require_dir "$BROWSER_HARNESS_SRC/browser_harness" "browser-harness source"

dataset_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$DATASET" | awk '{print $1}'
    return
  fi
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$DATASET" | awk '{print $1}'
    return
  fi
  echo "unavailable"
}

if [[ "$CONCURRENCY" -lt 1 ]]; then
  echo "CONCURRENCY must be >= 1" >&2
  exit 2
fi

if [[ "$JUDGE_CONCURRENCY" -lt 1 ]]; then
  echo "JUDGE_CONCURRENCY must be >= 1" >&2
  exit 2
fi

if [[ "$MAX_TURNS" -lt 10000 ]]; then
  echo "MAX_TURNS must stay >= 10000 for Internal_Bench_hard parity" >&2
  exit 2
fi

if [[ "$ULIMIT_NOFILE" -lt 1024 ]]; then
  echo "ULIMIT_NOFILE must be >= 1024" >&2
  exit 2
fi

if [[ "${#TASK_IDS[@]}" -gt 0 ]]; then
  declare -A seen_task_ids=()
  for task_id in "${TASK_IDS[@]}"; do
    if [[ -n "${seen_task_ids[$task_id]:-}" ]]; then
      echo "duplicate --task-id: $task_id" >&2
      exit 2
    fi
    seen_task_ids[$task_id]=1
  done
fi

EXPECTED_TOTAL_EFFECTIVE=106
if [[ "${#TASK_IDS[@]}" -gt 0 ]]; then
  EXPECTED_TOTAL_EFFECTIVE="${#TASK_IDS[@]}"
fi

NOFILE_TARGET="$ULIMIT_NOFILE"
NOFILE_HARD="$(ulimit -Hn)"
if [[ "$NOFILE_HARD" != "unlimited" && "$NOFILE_TARGET" -gt "$NOFILE_HARD" ]]; then
  NOFILE_TARGET="$NOFILE_HARD"
fi

if ! ulimit -n "$NOFILE_TARGET" 2>/dev/null; then
  actual_nofile="$(ulimit -n)"
  echo "warning: failed to raise open-file limit to $NOFILE_TARGET; current limit is $actual_nofile" >&2
fi

auth_status_connected() {
  local label="$1"
  if [[ ! -x "$BIN" ]]; then
    return 2
  fi
  "$BIN" auth status 2>/dev/null | grep -q "^${label}: connected"
}

auth_preflight() {
  local allow_defer="${1:-0}"
  local needs_defer=0
  local missing=()

  if [[ -z "${OPENAI_API_KEY:-}" && -z "${LLM_BROWSER_OPENAI_API_KEY:-}" ]]; then
    if ! auth_status_connected "OpenAI API key"; then
      if [[ "$allow_defer" == "1" && ! -x "$BIN" ]]; then
        needs_defer=1
      else
        missing+=("missing OPENAI_API_KEY or LLM_BROWSER_OPENAI_API_KEY, and no stored OpenAI key from auth login openai")
      fi
    fi
  fi

  if [[ -z "${BROWSER_USE_API_KEY:-}" ]]; then
    if ! auth_status_connected "Browser Use Cloud key"; then
      if [[ "$allow_defer" == "1" && ! -x "$BIN" ]]; then
        needs_defer=1
      else
        missing+=("missing BROWSER_USE_API_KEY, and no stored Browser Use Cloud key from auth login browser-use-cloud")
      fi
    fi
  fi

  if [[ "${#missing[@]}" -gt 0 ]]; then
    printf '%s\n' "${missing[@]}" >&2
    exit 1
  fi

  AUTH_PREFLIGHT_AFTER_BUILD="$needs_defer"
}

judge_preflight() {
  if [[ "$JUDGE_AFTER_RUN" != "1" ]]; then
    return 0
  fi

  if [[ ! -f "$REFERENCE_AGGREGATE" ]]; then
    echo "reference aggregate not found for --judge: $REFERENCE_AGGREGATE" >&2
    exit 1
  fi

  if ! command -v "$JUDGE_CLAUDE_BIN" >/dev/null 2>&1; then
    echo "judge claude binary not found for --judge: $JUDGE_CLAUDE_BIN" >&2
    exit 1
  fi
}

if [[ "$DRY_RUN" != "1" ]]; then
  auth_preflight 1
  judge_preflight
fi

if [[ -z "${OPENAI_API_KEY:-}" && -n "${LLM_BROWSER_OPENAI_API_KEY:-}" ]]; then
  export OPENAI_API_KEY="$LLM_BROWSER_OPENAI_API_KEY"
fi

BRANCH="$(git rev-parse --abbrev-ref HEAD | tr '/ .' '---')"
STAMP="$(date -u +%Y%m%d-%H%M%S)"
RUN_ID="${RUN_ID:-ibh-simple-harness-openai-${BRANCH}-${STAMP}}"
ROOT="${ROOT:-${OUT_BASE}/${RUN_ID}}"
STATE_DIR="$ROOT/state"
LOG_DIR="$ROOT/logs"
MANIFEST="$STATE_DIR/dataset-runs/$RUN_ID.json"
PACKETS="$ROOT/judge_packets.json"

cmd=(
  "$BIN"
  -c simple_harness=true
  -c codex_engine=true
  -c disable_local_search=true
  --state-dir "$STATE_DIR"
  dataset-run-openai "$DATASET"
)
if [[ "${#TASK_IDS[@]}" -gt 0 ]]; then
  for task_id in "${TASK_IDS[@]}"; do
    cmd+=(--task-id "$task_id")
  done
else
  cmd+=(--all)
fi
cmd+=(
  --model "$MODEL"
  --max-turns "$MAX_TURNS"
  --python-timeout-seconds "$PYTHON_TIMEOUT_SECONDS"
  --max-attempts 1
  --concurrency "$CONCURRENCY"
  --browser-mode cloud
  --run-id "$RUN_ID"
)

export BROWSER_HARNESS_SRC
export LLM_BROWSER_BROWSER_MODE=cloud
export LLM_BROWSER_AUTO_CHROME=0
export LLM_BROWSER_OPEN_CLOUD_LIVE_VIEW=0
export BROWSER_USE_EVAL_DONE_AUDIT="${BROWSER_USE_EVAL_DONE_AUDIT:-1}"
export BROWSER_USE_DISABLE_LOCAL_SEARCH="${BROWSER_USE_DISABLE_LOCAL_SEARCH:-1}"
export LLM_BROWSER_PROVIDER_MAX_RETRIES="${LLM_BROWSER_PROVIDER_MAX_RETRIES:-5}"
export BROWSER_USE_DATASET_TASK_TIMEOUT_SECONDS="$TASK_TIMEOUT_SECONDS"
export BROWSER_USE_DATASET_TASK_TOKEN_CAP="$TASK_TOKEN_CAP"
export BROWSER_USE_DATASET_TASK_SAFETY_POLL_SECONDS="$TASK_SAFETY_POLL_SECONDS"
unset BU_CDP_URL BU_CDP_WS BU_BROWSER_ID

if [[ "$DRY_RUN" == "1" ]]; then
  printf 'RUN_ID=%s\nROOT=%s\nDATASET=%s\nEXPECTED_TOTAL=%s\n' "$RUN_ID" "$ROOT" "$DATASET" "$EXPECTED_TOTAL_EFFECTIVE"
  if [[ "${#TASK_IDS[@]}" -gt 0 ]]; then
    printf 'TASK_IDS=%s\n' "$(IFS=,; echo "${TASK_IDS[*]}")"
  else
    printf 'TASK_IDS=all\n'
  fi
  printf 'command:'
  printf ' %q' "${cmd[@]}"
  printf '\n'
  if [[ "$JUDGE_AFTER_RUN" == "1" ]]; then
    printf 'judge_command: %q %q %q %q %q %q %q %q %q %q %q\n' \
      "$REPO_ROOT/scripts/judge-ibh-chunks-claude.py" \
      --judge-dir "$ROOT/judge" \
      --run-root "$ROOT" \
      --model "$JUDGE_MODEL" \
      --concurrency "$JUDGE_CONCURRENCY" \
      --claude-bin "$JUDGE_CLAUDE_BIN"
  fi
  exit 0
fi

mkdir -p "$LOG_DIR" "$STATE_DIR"

if [[ "$SKIP_BUILD" != "1" ]]; then
  cargo build -p browser-use-cli
fi

if [[ ! -x "$BIN" ]]; then
  echo "browser-use-terminal binary not found: $BIN" >&2
  exit 1
fi

if [[ "$AUTH_PREFLIGHT_AFTER_BUILD" == "1" ]]; then
  auth_preflight 0
fi

cat > "$ROOT/run-env.txt" <<EOF
run_id=$RUN_ID
root=$ROOT
repo=$REPO_ROOT
branch=$BRANCH
commit=$(git rev-parse HEAD)
dataset=$DATASET
dataset_sha256=$(dataset_sha256)
model=$MODEL
concurrency=$CONCURRENCY
max_turns=$MAX_TURNS
python_timeout_seconds=$PYTHON_TIMEOUT_SECONDS
task_timeout_seconds=$TASK_TIMEOUT_SECONDS
task_token_cap=$TASK_TOKEN_CAP
task_safety_poll_seconds=$TASK_SAFETY_POLL_SECONDS
expected_total=$EXPECTED_TOTAL_EFFECTIVE
task_ids=$([[ "${#TASK_IDS[@]}" -gt 0 ]] && (IFS=,; echo "${TASK_IDS[*]}") || echo all)
ulimit_nofile=$(ulimit -n)
browser_mode=cloud
simple_harness=true
codex_engine=true
disable_local_search=true
browser_harness_src=$BROWSER_HARNESS_SRC
judge_after_run=$JUDGE_AFTER_RUN
judge_model=$JUDGE_MODEL
judge_concurrency=$JUDGE_CONCURRENCY
reference_aggregate=$REFERENCE_AGGREGATE
openai_key_present=$([[ -n "${OPENAI_API_KEY:-}" || -n "${LLM_BROWSER_OPENAI_API_KEY:-}" ]] && echo true || echo false)
browser_use_key_present=$([[ -n "${BROWSER_USE_API_KEY:-}" ]] && echo true || echo false)
stored_openai_key_present=$("$BIN" auth status 2>/dev/null | grep -q '^OpenAI API key: connected' && echo true || echo false)
stored_browser_use_key_present=$("$BIN" auth status 2>/dev/null | grep -q '^Browser Use Cloud key: connected' && echo true || echo false)
EOF

echo "RUN_ID=$RUN_ID"
echo "ROOT=$ROOT"
echo "LOG=$LOG_DIR/dataset-run.log"

set +e
(
  set -o pipefail
  stdbuf -oL -eL "${cmd[@]}" 2>&1 | tee "$LOG_DIR/dataset-run.log"
) &
run_pid=$!

sleep "$HEALTH_AFTER_SECONDS"
if kill -0 "$run_pid" 2>/dev/null; then
  if [[ -f "$STATE_DIR/state.db" ]]; then
    sqlite3 -header -column "$STATE_DIR/state.db" "
      select
        count(*) as events,
        coalesce(sum(type='browser.connected'), 0) as browser_connected,
        coalesce(sum(payload_json like '%API_KEY missing%'), 0) as api_key_missing,
        coalesce(sum(type='dataset.case'), 0) as dataset_cases
      from events;
    " | tee "$ROOT/health-${HEALTH_AFTER_SECONDS}s.txt"
  else
    echo "state db not created after ${HEALTH_AFTER_SECONDS}s" | tee "$ROOT/health-${HEALTH_AFTER_SECONDS}s.txt"
  fi
fi

wait "$run_pid"
run_status=$?
set -e
final_status="$run_status"

if [[ -f "$MANIFEST" ]]; then
  jq '
    .selection as $selection
    | (reduce .sessions[] as $session ({}; .[$session.task_id] = $session)) as $latest_sessions
    | [
        $latest_sessions[]
        | . as $session
        | ([ $selection[]?
             | select(.task_id == $session.task_id)
             | (.confirmed_task // .task // .raw.confirmed_task // "")
           ][0] // "") as $task
        | {
            task_id: $session.task_id,
            ok: ($session.ok // false),
            task: $task,
            final_result: ($session.final_result // ""),
            session_id: ($session.session.id // $session.session_id // null),
            cwd: ($session.session.cwd // $session.cwd // null),
            artifact_root: ($session.session.artifact_root // $session.artifact_root // null),
            error: ($session.error // null)
          }
      ]
    | sort_by(.task_id)
  ' "$MANIFEST" > "$PACKETS"
  packet_count="$(jq 'length' "$PACKETS")"
  unique_packet_count="$(jq '[.[].task_id] | unique | length' "$PACKETS")"
  expected_count="$(jq '.summary.count // (.selection | length)' "$MANIFEST")"
  if [[ "$packet_count" != "$unique_packet_count" ]]; then
    echo "judge packet extraction produced duplicate task ids" >&2
    exit 1
  fi
  if [[ "$packet_count" != "$expected_count" ]]; then
    echo "judge packet count mismatch: packets=$packet_count expected=$expected_count" >&2
    exit 1
  fi
  jq -r '
    "runner_ok=\([.[] | select(.ok)] | length)/\(length) failed_ids=\([.[] | select(.ok == false) | .task_id] | join(","))"
  ' "$PACKETS" | tee "$ROOT/runner-summary.txt"
  echo "judge_packets=$PACKETS"
  "$REPO_ROOT/scripts/prepare-ibh-judge.py" \
    --run-root "$ROOT" \
    --run-id "$RUN_ID" \
    --packets "$PACKETS" \
    --state-db "$STATE_DIR/state.db" \
    --out-dir "$ROOT/judge" \
    --run-label "Internal_Bench_hard simple-harness OpenAI cloud"

  "$REPO_ROOT/scripts/audit-ibh-run-completion.py" \
    --run-root "$ROOT" \
    --run-id "$RUN_ID" \
    --expected-total "$EXPECTED_TOTAL_EFFECTIVE"

  if [[ "$JUDGE_AFTER_RUN" == "1" ]]; then
    judge_args=(
      "$REPO_ROOT/scripts/judge-ibh-chunks-claude.py"
      --judge-dir "$ROOT/judge"
      --run-root "$ROOT"
      --model "$JUDGE_MODEL"
      --concurrency "$JUDGE_CONCURRENCY"
      --claude-bin "$JUDGE_CLAUDE_BIN"
    )
    if [[ "$JUDGE_OVERWRITE" == "1" ]]; then
      judge_args+=(--overwrite)
    fi

    set +e
    (
      set -o pipefail
      "${judge_args[@]}" 2>&1 | tee "$LOG_DIR/judge-run.log"
    )
    judge_status=$?
    set -e
    if [[ "$judge_status" -ne 0 ]]; then
      echo "judge chunks failed with status $judge_status" >&2
      if [[ "$final_status" -eq 0 ]]; then
        final_status="$judge_status"
      fi
    else
      finalize_args=(
        "$REPO_ROOT/scripts/finalize-ibh-judged-run.sh"
        --run-root "$ROOT"
        --run-id "$RUN_ID"
        --judge-dir "$ROOT/judge"
        --reference-aggregate "$REFERENCE_AGGREGATE"
        --current-expected-total "$EXPECTED_TOTAL_EFFECTIVE"
        --reference-expected-total 106
      )
      if [[ "${#TASK_IDS[@]}" -gt 0 ]]; then
        for task_id in "${TASK_IDS[@]}"; do
          finalize_args+=(--task-id "$task_id")
        done
      fi

      set +e
      (
        set -o pipefail
        "${finalize_args[@]}" 2>&1 | tee "$LOG_DIR/judge-finalize.log"
      )
      finalize_status=$?
      set -e
      if [[ "$finalize_status" -ne 0 ]]; then
        echo "judge finalization failed with status $finalize_status" >&2
        if [[ "$final_status" -eq 0 ]]; then
          final_status="$finalize_status"
        fi
      else
        set +e
        "$REPO_ROOT/scripts/audit-ibh-run-completion.py" \
          --run-root "$ROOT" \
          --run-id "$RUN_ID" \
          --expected-total "$EXPECTED_TOTAL_EFFECTIVE" \
          --require-judged \
          2>&1 | tee "$LOG_DIR/completion-audit.log"
        audit_status=$?
        set -e
        if [[ "$audit_status" -ne 0 ]]; then
          echo "completion audit failed with status $audit_status" >&2
          if [[ "$final_status" -eq 0 ]]; then
            final_status="$audit_status"
          fi
        fi
      fi
    fi
  fi
else
  echo "manifest missing: $MANIFEST" >&2
fi

exit "$final_status"
