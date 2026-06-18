#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Verify the browser-harness supervisor architecture without running the live benchmark.

Default checks are credential-free and cover:
  - formatting / script syntax
  - eval wrapper no-80 + cloud/simple-harness dry-runs
  - fake 106-task run -> judge -> aggregate -> compare orchestration
  - completion-audit mechanics
  - focused Rust dataset/simple-harness tests

Use --full to also run the repo-owned terminal UI verifier.
Use --live-browser to also run the isolated local Chrome/CDP smoke. This starts
headless Chrome/Chromium with a temporary profile and does not use user profiles.

Usage:
  scripts/verify-browser-harness-supervisor.sh [--full] [--live-browser]
USAGE
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

FULL=0
LIVE_BROWSER=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --full)
      FULL=1
      shift
      ;;
    --live-browser)
      LIVE_BROWSER=1
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

section() {
  printf '\n== %s ==\n' "$1"
}

section "format"
cargo fmt --check
git diff --check

section "script syntax"
bash -n scripts/run-internal-bench-hard-openai.sh
bash -n scripts/finalize-ibh-judged-run.sh
bash -n scripts/live-browser-boundary-smoke.sh
python3 -m py_compile \
  scripts/audit-ibh-run-completion.py \
  scripts/judge-ibh-chunks-claude.py \
  scripts/prepare-ibh-judge.py \
  scripts/aggregate-ibh-judgments.py \
  scripts/compare-judged-runs.py

section "eval wrapper dry-run"
scripts/run-internal-bench-hard-openai.sh \
  --dry-run \
  --judge \
  --run-id supervisor-verify-dry-run \
  --root /tmp/but-supervisor-verify-dry-run

section "no-80 guard"
set +e
MAX_TURNS=80 scripts/run-internal-bench-hard-openai.sh \
  --dry-run \
  --run-id supervisor-verify-no80 \
  --root /tmp/but-supervisor-verify-no80 \
  >/tmp/but-supervisor-verify-no80.out \
  2>/tmp/but-supervisor-verify-no80.err
no80_status=$?
set -e
if [[ "$no80_status" -ne 2 ]]; then
  cat /tmp/but-supervisor-verify-no80.out
  cat /tmp/but-supervisor-verify-no80.err >&2
  echo "expected MAX_TURNS=80 dry-run to exit 2, got $no80_status" >&2
  exit 1
fi
if ! grep -q "MAX_TURNS must stay >= 10000" /tmp/but-supervisor-verify-no80.err; then
  cat /tmp/but-supervisor-verify-no80.err >&2
  echo "missing no-80 guard message" >&2
  exit 1
fi

section "python eval tooling tests"
uv run --with pytest python -m pytest -q \
  python/tests/test_audit_ibh_run_completion.py \
  python/tests/test_internal_bench_wrapper.py \
  python/tests/test_judge_ibh_chunks_claude.py \
  python/tests/test_prepare_ibh_judge.py \
  python/tests/test_simple_harness_artifact_audit.py

section "rust dataset/simple-harness tests"
cargo test -q -p browser-use-cli dataset_ -- --nocapture
cargo test -q -p browser-use-agent simple_harness -- --nocapture
cargo test -q -p browser-use-agent config_facade_codex_backend_missing_creds_is_honest_error -- --nocapture

PREVIOUS_RUN="/home/exedev/eval-runs/ibh-browser-harness-supervisor-goal-no80-openai-20260618-071302"
if [[ -d "$PREVIOUS_RUN" ]]; then
  section "previous judged run completion audit"
  scripts/audit-ibh-run-completion.py \
    --run-root "$PREVIOUS_RUN" \
    --run-id "$(basename "$PREVIOUS_RUN")" \
    --require-judged
fi

if [[ "$FULL" == "1" ]]; then
  section "terminal UI verifier"
  scripts/verify-terminal-ui.sh
fi

if [[ "$LIVE_BROWSER" == "1" ]]; then
  section "live browser smoke"
  export LLM_BROWSER_ALLOW_GOOGLE_CHROME="${LLM_BROWSER_ALLOW_GOOGLE_CHROME:-1}"
  export LLM_BROWSER_LIVE_STATE_DIR="${LLM_BROWSER_LIVE_STATE_DIR:-/tmp/but-live-browser-boundary-supervisor-$(date -u +%Y%m%d-%H%M%S)}"
  scripts/live-browser-boundary-smoke.sh
fi

section "auth status"
if [[ -x target/debug/browser-use-terminal ]]; then
  target/debug/browser-use-terminal auth status || true
else
  echo "target/debug/browser-use-terminal not built; skipped auth status"
fi

section "done"
echo "browser-harness supervisor verification passed"
