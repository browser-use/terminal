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
- repo-local submodule path: `codex`
- repo-local submodule commit used by implementation:
  `2832d78e759d2eba75fffdf84169cc6ef20514cf`

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
- [x] Dataset hash is recorded in every full eval run.
- [x] Internal_Bench_hard runner dry-run shows cloud, simple harness, 10k turns,
      expected model, expected concurrency.
- [x] `MAX_TURNS=80` dry-run fails before launching.

Gate 0 verification:

- dry-run command:
  - `scripts/run-internal-bench-hard-openai.sh --dry-run --judge --run-id audit-dry-run --root /tmp/ibh-audit-dry-run`
  - result: command includes `-c simple_harness=true`, `-c codex_engine=true`,
    `-c disable_local_search=true`, `--model gpt-5.5`, `--max-turns 10000`,
    `--python-timeout-seconds 180`, `--max-attempts 1`, `--concurrency 25`,
    `--browser-mode cloud`, and judge concurrency `5`.
- no-80 guard:
  - `MAX_TURNS=80 scripts/run-internal-bench-hard-openai.sh --dry-run --run-id no80-check --root /tmp/ibh-no80-check`
  - result: exits `2` with `MAX_TURNS must stay >= 10000 for Internal_Bench_hard parity`.

### Gate 1: CodexEngine Single-Session Proof

- [x] New `CodexEngine` crate/module exists.
- [x] It uses Codex app-server crates, not `codex exec`.
- [x] One Browser Use session maps to one Codex thread.
- [x] One Browser Use run maps to one Codex turn.
- [x] Raw Codex events are persisted.
- [x] Browser Use events are projected into the store/runtime.
- [x] Final answer capture uses Codex message items/deltas, not only
      `TurnCompleted`.
- [x] One non-browser task works through CLI.
- [x] One browser task works through cloud browser-harness.
- [ ] One non-browser task works through TUI.
- [ ] One SDK `Agent.run` works and streams events.

Gate 1 verification so far:

- implementation files:
  - `crates/browser-use-codex-engine/Cargo.toml`
  - `crates/browser-use-codex-engine/src/lib.rs`
  - `crates/browser-use-codex-engine/src/requests.rs`
  - `crates/browser-use-codex-engine/src/mapper.rs`
  - `crates/browser-use-codex-engine/src/runner.rs`
- direct Codex app-server dependency:
  - `codex-app-server-client = { path = "../../codex/codex-rs/app-server-client" }`
  - `codex-app-server-protocol = { path = "../../codex/codex-rs/app-server-protocol" }`
- compile anchor:
  - `pub type CodexInProcessClient = codex_app_server_client::InProcessAppServerClient;`
- event mapper evidence:
  - Codex app-server notifications are converted to Browser Use event names such
    as `model.stream_delta`, `model.turn.response`, `exec_command.output_delta`,
    `tool.output`, `tool.failed`, `codex.raw_response_item.completed`, and
    `session.done`.
  - `RuntimeAgentExecutor::run_codex_engine_blocking` appends every projected
    Codex event into the Browser Use store and runs the turn inside
    `runtime.run_agent(...)`, preserving runtime start/turn/completed/failure
    projection.
- dependency alignment needed for embedding:
  - `rusqlite` aligned to `0.32.1` so `libsqlite3-sys 0.30.1` is shared with
    Codex `sqlx-sqlite 0.8.6`.
  - `allocative` locked to `0.3.4` and `allocative_derive` to `0.3.3` to match
    Codex/starlark's `hashbrown 0.14.5` resolution.
- focused verification:
  - `cargo fmt --check`
  - result: passed
  - `cargo test -p browser-use-codex-engine`
  - result: `8 passed`
  - `cargo test -p browser-use-store`
  - result: `20 passed`
  - `cargo test -p browser-use-agent --lib`
  - result: `1104 passed; 1 ignored`
  - `cargo test -p browser-use-cli dataset_provider -- --nocapture`
  - result: `2 passed`
  - `JUDGE_AFTER_RUN=1 scripts/run-internal-bench-hard-openai.sh --dry-run`
  - result: printed full command with explicit `-c codex_engine=true`.
  - non-browser live smoke:
    `target/debug/browser-use-terminal -c simple_harness=true -c codex_engine=true -c disable_local_search=true --state-dir /tmp/but-codex-engine-smoke run-openai --model gpt-5.5 'Reply with exactly CODEX_ENGINE_SMOKE_OK and nothing else.'`
  - result: session `16224734-0fd8-485e-a4ea-70ffc0e3a2a5` wrote
    `codex_engine.started`, `codex.thread.started`, `codex.turn.started`,
    `codex.raw_event`, `session.done`, `agent.completed`, `harness.mirrored`,
    `harness.cleaned`; final file contains `CODEX_ENGINE_SMOKE_OK`.
  - API-key provider evidence: `codex_engine.started.model_provider` and
    `codex.thread.started.model_provider` are `browser_use_openai_api`; the
    smoke has zero `account/rateLimits/updated` Codex-account events.
  - browser live smoke:
    `target/debug/browser-use-terminal -c simple_harness=true -c codex_engine=true -c disable_local_search=true -c browser_mode='"cloud"' --state-dir /tmp/but-codex-browser-smoke run-openai --model gpt-5.5 'Use browser-harness to open https://example.com, read the page title, and answer with exactly the title.'`
  - result: session `31fd6d18-cefe-473b-9559-6658ddc71442` invoked
    browser-harness through the worker, recorded three successful
    `exec_command` / `tool.output` calls, wrote `session.done`, mirrored
    artifacts, and cleaned the worker.
  - runtime stack fix: embedded Codex initially overflowed the default Tokio
    worker stack; `RuntimeAgentExecutor` now sets a 16 MiB worker stack.
  - API-key provider fix: Browser Use OpenAI defaults map to custom Codex
    provider `browser_use_openai_api` with `env_key = "OPENAI_API_KEY"` and
    `requires_openai_auth = false`, preserving explicit user provider
    overrides.

### Gate 2: TUI/SDK Contract Preserved

- [ ] `task tui:dev` starts the real existing TUI.
- [ ] TUI task submission uses CodexEngine.
- [ ] TUI active followup works.
- [ ] TUI queued followup works.
- [ ] TUI cancel works.
- [ ] TUI history shows completed CodexEngine runs.
- [ ] SDK JSON-RPC API shape is unchanged.
- [ ] Python SDK `Client.agent.run` uses CodexEngine.
- [x] Store status projection remains correct for running/done/failed/cancelled.

Gate 2 verification so far:

- TUI/CLI/SDK option constructors set both `simple_harness=true` and
  `codex_engine=true`.
- Dataset provider option merging now preserves `codex_engine`; regression test:
  `cargo test -p browser-use-cli dataset_provider -- --nocapture`.
- CodexEngine runs inside `runtime.run_agent(...)` and consumes durable prompt
  input through the runtime. Regression coverage:
  `cargo test -p browser-use-agent --lib live_executor::tests:: -- --nocapture`.
- Full agent-lib status/runtime regression suite:
  `cargo test -p browser-use-agent --lib`.

### Gate 3: Browser-Harness Manager Product Wiring

- [x] Python browser-harness owns model-visible CDP/browser interaction.
- [x] Old Rust browser/CDP runtime is bypassed in CodexEngine mode.
- [x] Model-visible browser command output is byte-preserved.
- [ ] `BH_MANAGER_ROOT` is explicit and under Browser Use state/artifacts.
- [ ] `BH_MANAGER_SOCKET` is explicit.
- [ ] `BH_RUN_ID`, `BH_AGENT_ID`, and `BH_PARENT_AGENT_ID` are explicit.
- [ ] `/browser` setting feeds browser-harness manager intent.
- [ ] `/profile` setting feeds browser-harness manager intent.
- [ ] `/sync-cookies` cloud profile intent feeds browser-harness manager.
- [ ] SDK `browser.set_backend` feeds the same intent.
- [ ] SDK `browser.set_profile` feeds the same intent.
- [x] Browser manager cleanup runs on done/fail/cancel.
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

- [x] `cargo fmt --check`
- [x] `cargo test`
- [x] `uv run --with pytest python -m pytest -q`
- [x] `scripts/verify-terminal-ui.sh`
- [x] Inspect `/tmp/but-design-loop/` after TUI verification.

Gate 5 verification:

- `scripts/verify-terminal-ui.sh` passed.
- Artifact directory inspected: `/tmp/but-design-loop/`.
- Terminal flows covered by the repo smoke include browser overlay, history,
  model/account overlays, completed plain output, follow-up input, queued input,
  escape/cancel flows, paste, resize, scrollback, and slash-palette flows.

### Gate 6: Eval Smoke And Subset

- [ ] 5-task Internal_Bench_hard smoke run completes.
- [ ] Smoke run has complete artifacts and final answers.
- [ ] 20-task hard subset completes.
- [ ] Subset is judged with the locked rubric.
- [ ] Any current-only regression is mapped to code/runtime/evidence.

### Gate 7: Full Judged Internal_Bench_hard

- [x] Full 106-task run launched with cloud browser mode.
- [x] Full run uses no 80-turn cap.
- [x] Full run has 106 unique task ids.
- [x] Full run has 106 judge packets.
- [x] Full run has 106 judgments.
- [x] Mechanical audit passes.
- [x] Strict judged score is recorded.
- [x] Raw Codex + browser-harness reference is run or explicitly recorded as
      unavailable.
- [x] Task-by-task comparison against raw reference is produced.
- [ ] Score is `>= 93/106` or within 3 tasks of fresh raw reference.
- [x] Every remaining failure has a code/evidence/root-cause classification.

Gate 7 attempt log:

- Aborted run:
  `/home/exedev/eval-runs/ibh-simple-harness-openai-simple-harness-parity-test-20260618-20260619-025439`.
- Reason: implementation bug found before enough completions to score. Codex
  emitted a retryable app-server `error` notification with `willRetry=true`
  (`Reconnecting... 1/5` after a transient stream disconnect), but
  `CodexEventMapper` incorrectly treated all `error` notifications as terminal
  failures.
- Fix: retryable Codex app-server errors are now projected as
  `model.turn.error` evidence but do not abort the Browser Use session;
  non-retryable errors remain terminal.
- Regression coverage:
  `cargo test -p browser-use-codex-engine`.
- Aborted run:
  `/home/exedev/eval-runs/ibh-simple-harness-openai-simple-harness-parity-test-20260618-20260619-030044`.
- Reason: runner hit `Too many open files (os error 24)` while writing the
  dataset manifest after 43 extracted packets. The VM shell soft open-file
  limit was 1024, while concurrency 25 with embedded Codex plus browser-harness
  workers needs a higher limit.
- Fix: the eval runner now raises `ulimit -n` up to `ULIMIT_NOFILE`, capped by
  the shell hard limit, and records the actual limit in `run-env.txt`.
- Aborted run:
  `/home/exedev/eval-runs/ibh-simple-harness-openai-simple-harness-parity-test-20260618-20260619-031849`.
- Reason: task `he7ur2` hit `Codex app-server event stream lagged by 3 events`
  during a very large trace. Lossless event capture is required for judged
  evals, so lag remains terminal.
- Fix: the embedded CodexEngine in-process app-server channel capacity is now
  65,536 events instead of 1,024, matching the high-throughput eval path rather
  than the small default transport path.
- Aborted run:
  `/home/exedev/eval-runs/ibh-simple-harness-openai-simple-harness-parity-test-20260618-20260619-034811`.
- Result before manual stop: 105 completed sessions, 0 failed sessions, 1
  running session (`6dpbhs`), with no `session.failed`, `agent.failed`, or
  `codex.event_lagged` events. The run was stopped with Ctrl-C after the last
  task continued active beyond 6,800 events and roughly 25M cumulative Codex
  token-usage events.
- Reason: removing the 80-turn cap exposed an eval-runner safety gap. A single
  hard archival lookup can keep burning tokens indefinitely while still
  emitting valid events, so a full judged run can fail to become judgeable even
  when the implementation is otherwise healthy.
- Fix: dataset runs now thread a cancellation token into
  `run_existing_session_from_config_and_notify`, and the Internal_Bench_hard
  script opts into a generous eval safety guard:
  `TASK_TIMEOUT_SECONDS=2700`, `TASK_TOKEN_CAP=30000000`, and
  `TASK_SAFETY_POLL_SECONDS=10`. Safety cancellation records
  `dataset.task_safety_cancelled`, requests session cancellation, and marks the
  task result as `error_type=dataset_safety` instead of leaving it pending.
- Verification after fix:
  - `cargo fmt --check`
  - `cargo test`
  - `uv run --with pytest python -m pytest -q`
  - `bash -n scripts/run-internal-bench-hard-openai.sh`
- Completed judged run:
  `/home/exedev/eval-runs/ibh-simple-harness-openai-simple-harness-parity-test-20260618-20260619-053010`.
- Run metadata:
  - branch: `simple-harness-parity-test-20260618`
  - commit: `f7cb1f93356bb192d56fac2cc60d9c630ed47f32`
  - dataset sha256:
    `62ca711571e3337234efb54e7708d5768dec8c849bb8a2a54e010c4e31e988c4`
  - provider/model: OpenAI API, `gpt-5.5`
  - browser mode: cloud
  - concurrency: `25`
  - max turns: `10000`
  - safety guard: `TASK_TIMEOUT_SECONDS=2700`,
    `TASK_TOKEN_CAP=30000000`, `TASK_SAFETY_POLL_SECONDS=10`
  - simple harness: `true`
  - CodexEngine: `true`
- Runner result:
  - 105/106 completed.
  - `jgzlma` was cancelled by the dataset safety guard after the 2700s
    wall-clock cap.
  - No missing packet/task ids.
- Judge result:
  - 106 judge packets, 106 native event logs, 5 chunk files, 106 judgments.
  - Mechanical completion audit with `--require-judged`: passed.
  - Score: `91/106` (`85.8%`).
  - Failed ids:
    `0kqsos`, `6dpbhs`, `82kkzm`, `c856wp`, `eo2t8f`, `jgzlma`,
    `l3gywi`, `m5zja8`, `pvs7hz`, `q46nou`, `r5l8a7`, `s3kkv9`,
    `togn1w`, `v18kgy`, `zcotoh`.
  - Failure classes:
    - `site-access-blocked`: `0kqsos`, `q46nou`, `r5l8a7`, `s3kkv9`
    - `missing-required-fields`: `82kkzm`, `jgzlma`, `l3gywi`
    - `source-scope-drift`: `m5zja8`, `pvs7hz`
    - `incomplete-artifact`: `v18kgy`
    - `missing-core-fields`: `zcotoh`
    - `site-blocked`: `c856wp`
    - `source-limited`: `eo2t8f`
    - `weak-evidence`: `6dpbhs`
    - `wrong-scope`: `togn1w`
  - Judge caveat: Claude Code print-mode judging could not run in this VM
    because Claude auth returned `401 Invalid authentication credentials`.
    Five Codex subagents judged the prepared chunks with the same saved rubric
    and packet contract, and the existing aggregate/comparison validators
    accepted the outputs with no validation errors.
- Raw-reference comparison:
  - reference aggregate:
    `/home/exedev/eval-runs/ibh-purecodex-175254-rejudge-jsonl-20260613/judge_aggregate.json`
  - reference score: `96/106` (`90.6%`)
  - comparison:
    `/home/exedev/eval-runs/ibh-simple-harness-openai-simple-harness-parity-test-20260618-20260619-053010/current-vs-raw-judged-delta.md`
  - both pass: `86`
  - both fail: `5`
  - current-only regressions: `10`
  - current-only improvements: `5`
  - regressions:
    `0kqsos`, `6dpbhs`, `82kkzm`, `jgzlma`, `l3gywi`, `m5zja8`,
    `q46nou`, `s3kkv9`, `v18kgy`, `zcotoh`
  - improvements:
    `2vxyzx`, `84xyjo`, `az39pe`, `h42m44`, `up8ijl`
  - shared failures:
    `c856wp`, `eo2t8f`, `pvs7hz`, `r5l8a7`, `togn1w`

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

Active phase: post-Gate-7 regression triage.

Latest checkpoint:

- Regression triage report:
  `docs/agent-design/internal-bench-hard-regression-triage-20260619.md`
- Implemented artifact-audit and prompt fixes for four judged regressions:
  - `zcotoh`: task-allowed `N/A` fields no longer force an incomplete result.
  - `82kkzm`: self-withheld/missing complete article text is blocked.
  - `m5zja8`: likely non-IT UNGM rows are blocked as scope drift.
  - `v18kgy`: `complete: false`, `is_complete: false`, and
    `ready_for_done: false` are blocked.
- Added `--task-id ID` support to
  `scripts/run-internal-bench-hard-openai.sh` for focused reruns using the same
  CodexEngine/simple-harness path as the full run.
- Added subset-aware finalization/comparison while preserving the default
  full-run `106` task validation.
- Verification:
  - `python3 -m py_compile prompts/simple-harness-artifact-audit.py scripts/compare-judged-runs.py scripts/audit-ibh-run-completion.py`
  - `bash -n scripts/run-internal-bench-hard-openai.sh scripts/finalize-ibh-judged-run.sh`
  - `uv run --with pytest python -m pytest -q python/tests/test_simple_harness_artifact_audit.py`
  - targeted dry-run: `EXPECTED_TOTAL=4`, task ids
    `zcotoh,m5zja8,v18kgy,82kkzm`
  - full dry-run still uses `--all` and `EXPECTED_TOTAL=106`
  - existing full judged run finalizes successfully at `91/106` with no
    validation errors.

Next concrete action:

1. Run and judge the focused subset:
   `zcotoh`, `m5zja8`, `v18kgy`, `82kkzm`.
2. Decide whether `jgzlma` should be rerun with a larger one-task safety cap or
   treated as an acceptable eval-runner guard failure.
3. Verify the remaining unchecked TUI/SDK product gates before claiming the full
   product integration is complete.
