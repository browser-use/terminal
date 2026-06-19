# Codex Engine + Browser-Harness Execution Tracker

Created: 2026-06-19T01:47:14Z

This tracker turns the execution plan into gates. A phase is not complete until
its implementation, tests, and evidence are present. Do not mark the goal done
from a working TUI alone.

Plan doc:

- `docs/agent-design/codex-engine-browser-harness-execution-plan.md`

## Baseline Snapshot

Browser Use Terminal repo:

- branch: `simple-harness-parity-test-20260618`
- commit: `0fd385bf7553aa24838c749e6f5ed0e5f35db4a2`
- current local additions:
  - `docs/agent-design/codex-engine-browser-harness-execution-plan.md`
  - `docs/agent-design/codex-engine-browser-harness-execution-tracker.md`
- unrelated/unclassified untracked files present before this tracker:
  - `result.csv`
  - `result.json`
  - `result.md`
  - `results.txt`

Codex fork:

- path: `/home/exedev/repos/codex`
- commit: `d735ef162f538d8e571778182a51ad5e9f795dfb`
- status at snapshot: clean

Browser-harness:

- path: `/home/exedev/repos/browser-harness`
- initial inspected commit: `785fb47b26e168862f3ae4546d4746602b523a7c`
- pinned snapshot branch: `but-manager-profile-snapshot-20260619`
- pinned snapshot commit: `91dbc6b1340c029bb841f7824fb74bb64434ab48`
- status after pinning: clean
- files included in the pinned snapshot:
  - `agent-workspace/agent_helpers.py`
  - `src/browser_harness/admin.py`
  - `src/browser_harness/daemon.py`
  - `src/browser_harness/run.py`
  - `tests/unit/test_admin.py`
  - `tests/unit/test_daemon_profile_context.py`
  - `tests/unit/test_run.py`
  - `docs/browser-manager-final-plan.md`
  - `docs/browser-manager-implementation-plan.md`
  - `docs/browser-manager-interface-plan.md`
  - `docs/browser-manager-v2-plan.md`
- focused verification before pinning:
  - `uv run --with pytest python -m pytest -q tests/unit/test_admin.py tests/unit/test_daemon_profile_context.py tests/unit/test_run.py`
  - result: `76 passed in 0.32s`

Dataset:

- path: `/home/exedev/datasets/Internal_Bench_hard.json`
- sha256: `62ca711571e3337234efb54e7708d5768dec8c849bb8a2a54e010c4e31e988c4`

## Completion Gates

### Gate 0: Source And Eval Inputs Frozen

- [x] Browser Use Terminal planning/tracker branch is committed.
- [x] Codex fork baseline commit is recorded.
- [x] Browser-harness commit is pinned.
- [x] Browser-harness dirty changes are either committed or archived in eval
      metadata.
- [x] Dataset hash is recorded in this tracker.
- [ ] Dataset hash is recorded in every full eval run.
- [x] Internal_Bench_hard runner dry-run shows cloud, simple harness, 10k turns,
      expected model, expected concurrency.
- [x] `MAX_TURNS=80` dry-run fails before launching.

Gate 0 verification:

- dry-run command:
  - `scripts/run-internal-bench-hard-openai.sh --dry-run --judge --run-id audit-dry-run --root /tmp/ibh-audit-dry-run`
  - result: command includes `-c simple_harness=true`, `-c disable_local_search=true`, `--model gpt-5.5`, `--max-turns 10000`, `--python-timeout-seconds 180`, `--max-attempts 1`, `--concurrency 25`, `--browser-mode cloud`, and judge concurrency `5`.
- no-80 guard:
  - `MAX_TURNS=80 scripts/run-internal-bench-hard-openai.sh --dry-run --run-id no80-check --root /tmp/ibh-no80-check`
  - result: exits `2` with `MAX_TURNS must stay >= 10000 for Internal_Bench_hard parity`.

### Gate 1: CodexEngine Single-Session Proof

- [ ] New `CodexEngine` crate/module exists.
- [ ] It uses Codex app-server crates, not `codex exec`.
- [ ] One Browser Use session maps to one Codex thread.
- [ ] One Browser Use run maps to one Codex turn.
- [ ] Raw Codex events are persisted.
- [ ] Browser Use events are projected into the store/runtime.
- [ ] Final answer capture uses Codex message items/deltas, not only
      `TurnCompleted`.
- [ ] One non-browser task works through TUI.
- [ ] One browser task works through cloud browser-harness.
- [ ] One SDK `Agent.run` works and streams events.

### Gate 2: TUI/SDK Contract Preserved

- [ ] `task tui:dev` starts the real existing TUI.
- [ ] TUI task submission uses CodexEngine.
- [ ] TUI active followup works.
- [ ] TUI queued followup works.
- [ ] TUI cancel works.
- [ ] TUI history shows completed CodexEngine runs.
- [ ] SDK JSON-RPC API shape is unchanged.
- [ ] Python SDK `Client.agent.run` uses CodexEngine.
- [ ] Store status projection remains correct for running/done/failed/cancelled.

### Gate 3: Browser-Harness Manager Product Wiring

- [ ] Python browser-harness owns model-visible CDP/browser interaction.
- [ ] Old Rust browser/CDP runtime is bypassed in CodexEngine mode.
- [ ] Model-visible browser command output is byte-preserved.
- [ ] `BH_MANAGER_ROOT` is explicit and under Browser Use state/artifacts.
- [ ] `BH_MANAGER_SOCKET` is explicit.
- [ ] `BH_RUN_ID`, `BH_AGENT_ID`, and `BH_PARENT_AGENT_ID` are explicit.
- [ ] `/browser` setting feeds browser-harness manager intent.
- [ ] `/profile` setting feeds browser-harness manager intent.
- [ ] `/sync-cookies` cloud profile intent feeds browser-harness manager.
- [ ] SDK `browser.set_backend` feeds the same intent.
- [ ] SDK `browser.set_profile` feeds the same intent.
- [ ] Browser manager cleanup runs on done/fail/cancel.
- [ ] Public browser evidence excludes secrets, CDP URLs, and provider browser
      IDs.

### Gate 4: Required Codex Fork Support

- [ ] Lossless raw event path exists or audited runs fail on event lag.
- [ ] Thread/session shell env is not process-global.
- [ ] Browser-harness skill text/root is thread-scoped or safely injected.
- [ ] Codex auth/config home is separate from browser-harness runtime state.
- [ ] Provider fixed-per-session behavior is documented, or per-turn provider
      switching is implemented.
- [ ] Auth refresh behavior is tested or explicitly out of scope for eval path.

### Gate 5: Local Verification

- [ ] `cargo fmt --check`
- [ ] `cargo test`
- [ ] `uv run --with pytest python -m pytest -q`
- [ ] `scripts/verify-terminal-ui.sh`
- [ ] Inspect `/tmp/but-design-loop/` after TUI verification.

### Gate 6: Eval Smoke And Subset

- [ ] 5-task Internal_Bench_hard smoke run completes.
- [ ] Smoke run has complete artifacts and final answers.
- [ ] 20-task hard subset completes.
- [ ] Subset is judged with the locked rubric.
- [ ] Any current-only regression is mapped to code/runtime/evidence.

### Gate 7: Full Judged Internal_Bench_hard

- [ ] Full 106-task run launched with cloud browser mode.
- [ ] Full run uses no 80-turn cap.
- [ ] Full run has 106 unique task ids.
- [ ] Full run has 106 judge packets.
- [ ] Full run has 106 judgments.
- [ ] Mechanical audit passes.
- [ ] Strict judged score is recorded.
- [ ] Raw Codex + browser-harness reference is run or explicitly recorded as
      unavailable.
- [ ] Task-by-task comparison against raw reference is produced.
- [ ] Score is `>= 93/106` or within 3 tasks of fresh raw reference.
- [ ] Every remaining failure has a code/evidence/root-cause classification.

## Stop Conditions

Stop and do not claim completion if any of these occur:

- Browser-harness dirty state is not recorded in an eval run.
- Dataset hash differs from the baseline and the run is still compared to the
  old score.
- The model-visible surface includes old Browser Use browser tools in benchmark
  mode.
- Rust rewrites, summarizes, truncates, or normalizes browser-harness command
  output.
- Final answer capture is incomplete or stdout-tail based.
- Full eval has fewer than 106 packets or fewer than 106 judgments.
- Judge prompt/rubric differs between compared runs.
- Current-only regressions are hand-waved as variance without artifacts.

## Current Status

Active phase: Gate 1.

Next concrete action:

1. Start `CodexEngine` single-session implementation.
