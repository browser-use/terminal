# Browser-Harness Supervisor Completion Audit - 2026-06-18

## Objective

Implement the browser-harness supervisor architecture end to end:

- Rust supervises TUI, SDK, session, store, and process lifecycle.
- Python browser-harness owns CDP/browser interaction.
- Codex sees the raw browser-harness interface.
- Previous terminal browser/profile/product behavior is restored.
- `Internal_Bench_hard` is judged task by task against raw Codex +
  browser-harness before the goal is complete.

Current worktree:

- Repo: `/home/exedev/new-core/main`
- Branch: `main`
- Commit: `063db5820db9e2bf8827c87f63d0e315dba6b0ad`
- External browser-harness source: `/home/exedev/repos/browser-harness`

## Score Snapshot

| Run | Score | Evidence | Status |
| --- | ---: | --- | --- |
| Current simple-harness implementation | `90/106` | `docs/eval-journal/current-vs-raw-judged-delta-20260618.md` | Last judged score |
| Raw Codex + browser-harness reference | `96/106` | Same comparison report | Reference target |
| New full run after latest product/profile fixes | Not run | No model/cloud credentials in this shell | Missing |

## Completion Audit

| Requirement | Evidence inspected | Status |
| --- | --- | --- |
| Rust supervises TUI/session/store/runtime lifecycle | `scripts/verify-terminal-ui.sh` passed; Rust runtime/session tests passed inside verifier; TUI smoke artifacts in `/tmp/but-design-loop/` | Proven for tested paths |
| SDK uses Rust runtime, not direct CDP ownership | SDK JSON-RPC tests in `crates/browser-use-cli/src/main.rs`; Python `browser_use` package source under `python/browser_use`; `Client.browser.set_backend`, `Client.browser.set_profile`, `Client.agent.run`, `Agent.run`, and `Browser.start` paths covered | Proven for tested SDK paths |
| Python browser-harness owns browser/CDP in simple-harness mode | `crates/browser-use-agent/src/simple_harness.rs`; worker/shim tests; browser-harness Python tests under `/home/exedev/repos/browser-harness/tests/unit` | Proven for simple-harness path |
| Codex sees raw browser-harness interface | `simple_harness_uses_codex_like_tool_surface`, `simple_harness_tool_search_does_not_leak_hidden_tools`, prompt and shim tests | Proven by unit tests |
| Raw command semantics are preserved | `browser_harness_shim_preserves_stdout_stderr_exit_and_stdin`, fixed-trace worker parity tests | Proven by unit tests |
| Worker events do not pollute model-visible output | `harness.prepared` / worker event JSONL behavior and simple-harness tests | Proven by unit tests |
| Fresh eval/run state can use product-global auth | Provider resolution reads env, current run store, then default terminal auth store for `auth.*.api_key`; SDK/harness cloud env injection does the same for `auth.browser_use_cloud.api_key`; wrapper preflight accepts env or `auth status` stored keys | Proven by focused unit tests and script preflight checks |
| Cloud benchmark mode is forced through browser-harness, not local Chrome | `scripts/run-internal-bench-hard-openai.sh --dry-run` emits `--browser-mode cloud`; env clears `BU_CDP_URL`, `BU_CDP_WS`, `BU_BROWSER_ID` | Proven for runner construction |
| No 80-turn cap on `Internal_Bench_hard` | CLI tests `internal_bench_hard_*`; script rejects `MAX_TURNS < 10000`; dry-run emits `--max-turns 10000` | Proven |
| Browser Use cloud profile id/name reaches browser-harness cloud creation | simple-harness shell-shim tests and Python `test_run.py` cloud bootstrap tests | Proven by unit tests |
| Stale cloud daemon is restarted before raw harness command | simple-harness stale remote daemon test | Proven by unit test |
| `/browser` product flow restored | TUI browser selection tests and `scripts/verify-terminal-ui.sh` browser dump/smoke artifacts | Proven for tested paths |
| `/profile` product flow restored | TUI default-profile tests; local Chrome profile snapshot tests; Python marker attach tests | Proven for tested paths |
| Existing chats keep their browser/profile snapshot | `selected_session_keeps_browser_profile_while_new_task_uses_latest_default` | Proven by unit test |
| Local Chrome without default profile blocks before arbitrary attach | Python `test_product_local_mode_without_profile_blocks_before_daemon`; simple-harness/product-local tests | Proven by unit test |
| Wrong-profile local attach is blocked | Python marker/context tests reject arbitrary/cross-context attach | Proven by unit test |
| Local remote-debugging setup states are classified | Python `test_admin.py` covers HTTP 403 permission popup and `DevToolsActivePort` checkbox-off messages | Proven by unit test |
| End-to-end local Chrome recovery in a real browser | `scripts/verify-browser-harness-supervisor.sh --full --live-browser` passed; it ran `scripts/live-browser-boundary-smoke.sh` against isolated headless Chrome/Chromium with a temporary profile and dedicated CDP port; state root `/tmp/but-live-browser-boundary-supervisor-20260618-125745` contains successful browser events, capture frames, copied `report.csv`, and stale-target recovery output | Proven for isolated local Chrome/CDP smoke |
| `/sync-cookies` remains product-side | CLI/TUI sync-cookie tests; verifier passed | Proven for tested paths |
| `/task`, `/history`, `/model`, `/context`, `/goal`, `/feedback` remain product-side | Command palette/action tests, goal/context/model/history tests, feedback surface test, verifier artifacts | Proven for tested paths |
| `/domains` reaches raw harness without Rust browser tool exposure | generated `agent_helpers.py` guards plus simple-harness domain tests | Proven by unit tests |
| `/email` reaches raw harness through product store helpers | generated helpers and CLI email tests | Proven by unit tests |
| `/secrets` and `/import-passwords` reach raw harness through redacted bridge | generated helpers, worker socket resolver, CLI/TUI secrets tests | Proven by unit tests |
| TUI behavior is verified in a real terminal | `scripts/verify-terminal-ui.sh` passed; inspected `/tmp/but-design-loop` captures | Proven for tested flows |
| Simple-harness prompt gives the model a pre-finalization artifact checker | `prompts/dataset-case-simple-harness-user.md` now names `artifact-audit`; prompt tests still forbid `browser_script`, `done(`, and `audit_artifact`; replay against known failed artifacts catches `8hyexf`, `mly4ly`, `l3gywi`, `q85jsg`, `swebnv`, `h42m44`, `82kkzm`, `afeyuh`, `y72ivg`, and mirrored `jgzlma` shapes | Proven for targeted failure classes |
| Dataset runner preserves explicit final deliverables as judge-visible artifacts | `dataset_attempt_result` mirrors valid JSON-shaped `session.done.result` into `cwd/result.json` when the task asks for JSON/schema output, and mirrors explicit list/table/marker finals into `cwd/result.txt` when the task asks for that text format, but only when no canonical result file exists; tests cover JSON mirroring, text marker mirroring, no overwrite, and no mirroring for plain text identity answers | Proven by unit tests |
| Judge prep handles current SQLite events and raw per-task JSONL events | `python/tests/test_prepare_ibh_judge.py`; `scripts/prepare-ibh-judge.py` generated 106 packets, 5 chunks, and 106 normalized native event logs for both `/tmp/ibh-sqlite-judge-smoke` and `/tmp/ibh-jsonl-judge-fallback` | Proven for offline judge-prep paths |
| Locked chunk judging can be launched reproducibly | `scripts/judge-ibh-chunks-claude.py` discovers prepared `packets_*.json`, launches one Claude Code print-mode judge per chunk, validates strict JSON rows against packet task ids, writes `chunk_*.json`, and can be called by `scripts/run-internal-bench-hard-openai.sh --judge`; fake-Claude tests cover chunk execution and validation; wrapper tests cover cloud/10k/no-local dry-run command construction, 80-turn rejection, env-file judge defaults, missing credential reporting, early `--judge` dependency failures for missing Claude binary/reference aggregate, and a non-dry fake 106-task run through packet extraction, judge prep, chunk judging, aggregation, and comparison | Proven by focused unit tests and fake end-to-end wrapper run |
| Completed run roots are mechanically audited before trust | `scripts/audit-ibh-run-completion.py` verifies manifest selection/session task counts, `judge_packets.json`, `packets_all.json`, packet chunks, native event logs, judge chunks, `judge_aggregate.json`, and `current-vs-raw-judged-delta.md`; the Internal_Bench_hard wrapper now calls it after packet prep and again after judged finalization with `--require-judged` | Proven by focused unit tests, fake wrapper run, and previous real run audit |
| Supervisor architecture/eval readiness has a single verifier | `scripts/verify-browser-harness-supervisor.sh --full --live-browser` runs formatting, script syntax, cloud/simple-harness/10k dry-run, no-80 guard, Python eval tooling tests, Rust dataset/simple-harness tests, prior judged-run completion audit, the full repo-owned terminal UI verifier, isolated local Chrome/CDP smoke, and auth status | Proven by combined full/live verifier run |
| Judge aggregation/finalizer validates strict chunk output and compares against raw reference | Copied judged artifacts under `/tmp/ibh-aggregate-smoke`; aggregate had 106 rows, no validation errors; finalizer produced `/tmp/ibh-aggregate-smoke/comparison.md` against the raw reference aggregate | Proven for offline finalizer path |
| Internal_Bench_hard subset judged after latest fixes | No credentialed run available in current shell | Missing |
| Internal_Bench_hard full 106 judged after latest fixes | No credentialed run available in current shell | Missing |
| Task-by-task comparison against raw Codex + browser-harness after latest fixes | Judge/finalizer tooling is validated on previous artifacts, but no new judged run exists | Missing |

## Current Blockers

- This shell has no `OPENAI_API_KEY` or `LLM_BROWSER_OPENAI_API_KEY`.
- This shell has no `BROWSER_USE_API_KEY`.
- There is no repo `.env` in `/home/exedev/new-core/main`.
- `auth status` shows no stored OpenAI key and no stored Browser Use Cloud key
  in `/home/exedev/.browser-use-terminal`.

The runner now loads `$ENV_FILE` / repo `.env` if present and can use stored
terminal auth from `browser-use-terminal auth login openai` plus
`browser-use-terminal auth login browser-use-cloud`, so a credentialed
continuation should run:

```bash
scripts/run-internal-bench-hard-openai.sh --run-id <run-id> --root <fresh-root> --judge
```

It will build a cloud-only simple-harness run with `--max-turns 10000`,
`--concurrency 25`, prepare judge packets, run the five locked judge chunks,
aggregate them, and write `$ROOT/current-vs-raw-judged-delta.md`.

## Validation Run For This Audit

- `bash -n scripts/run-internal-bench-hard-openai.sh`
- `scripts/verify-browser-harness-supervisor.sh`
- `scripts/verify-browser-harness-supervisor.sh --full`
- `scripts/verify-browser-harness-supervisor.sh --live-browser`
- `scripts/verify-browser-harness-supervisor.sh --full --live-browser`
- `CHROME_PATH=/bin/google-chrome LLM_BROWSER_LIVE_STATE_DIR=/tmp/but-live-browser-boundary-20260618-124701 scripts/live-browser-boundary-smoke.sh`
- `scripts/run-internal-bench-hard-openai.sh --dry-run --run-id audit-dry-run --root /tmp/but-audit-dry-run`
- `scripts/run-internal-bench-hard-openai.sh --dry-run --judge --run-id judge-dry-run --root /tmp/but-judge-dry-run`
- `scripts/run-internal-bench-hard-openai.sh --run-id credential-preflight-<timestamp> --root /tmp/but-credential-preflight-<timestamp>`
- `MAX_TURNS=80 scripts/run-internal-bench-hard-openai.sh --dry-run --run-id no80-check --root /tmp/but-no80-check`
- `cargo test -q -p browser-use-cli internal_bench_hard -- --nocapture`
- `cargo test -q -p browser-use-cli sdk_json_rpc_browser_settings_feed_stored_defaults_to_run_config -- --nocapture`
- `cargo test -q -p browser-use-cli sdk_json_rpc_agent_run_task_executes_fake_backend_with_normalized_history -- --nocapture`
- `cargo test -q -p browser-use-cli sdk_json_rpc_agent_run_executes_fake_backend -- --nocapture`
- `cargo test -q -p browser-use-agent auth_store -- --nocapture`
- `cargo test -q -p browser-use-agent key_is_fallback -- --nocapture`
- `cargo test -q -p browser-use-cli cloud_credentials -- --nocapture`
- `cargo test -q -p browser-use-cli default_auth_store -- --nocapture`
- `cargo test -q -p browser-use-cli stored_cloud_key -- --nocapture`
- `cargo test -q -p browser-use-cli simple_harness_dataset_prompt_matches_shell_browser_harness_surface -- --nocapture`
- `cargo test -q -p browser-use-cli simple_harness_dataset_session_persists_simple_prompt -- --nocapture`
- `cargo test -q -p browser-use-cli dataset_artifact_audit -- --nocapture`
- `cargo test -q -p browser-use-cli dataset_ -- --nocapture`
- `uv run --with pytest python -m pytest -q python/tests/test_simple_harness_artifact_audit.py`
- `uv run --with pytest python -m pytest -q python/tests/test_audit_ibh_run_completion.py`
- `uv run --with pytest python -m pytest -q python/tests/test_prepare_ibh_judge.py`
- `uv run --with pytest python -m pytest -q python/tests/test_judge_ibh_chunks_claude.py`
- `uv run --with pytest python -m pytest -q python/tests/test_internal_bench_wrapper.py`
- `uv run --with pytest python -m pytest -q python/tests/test_internal_bench_wrapper.py::test_internal_bench_wrapper_can_run_prepare_judge_and_finalize_with_fakes`
- `uv run --with pytest python -m pytest -q python/tests`
- `cargo fmt --check`
- `scripts/verify-terminal-ui.sh`
- `cargo test -q -p browser-use-tui feedback_palette_action_opens_feedback_surface -- --nocapture`
- `cd /home/exedev/repos/browser-harness && uv run --with pytest python -m pytest -q tests/unit/test_admin.py tests/unit/test_daemon_profile_context.py tests/unit/test_run.py`
- `python3 -m py_compile scripts/audit-ibh-run-completion.py scripts/judge-ibh-chunks-claude.py scripts/prepare-ibh-judge.py scripts/aggregate-ibh-judgments.py scripts/compare-judged-runs.py`
- `scripts/audit-ibh-run-completion.py --run-root /home/exedev/eval-runs/ibh-browser-harness-supervisor-goal-no80-openai-20260618-071302 --run-id ibh-browser-harness-supervisor-goal-no80-openai-20260618-071302 --require-judged`
- `scripts/judge-ibh-chunks-claude.py --judge-dir /home/exedev/eval-runs/ibh-browser-harness-supervisor-goal-no80-openai-20260618-071302/judge --run-root /home/exedev/eval-runs/ibh-browser-harness-supervisor-goal-no80-openai-20260618-071302 --dry-run`
- `scripts/prepare-ibh-judge.py --run-root /home/exedev/eval-runs/ibh-rustcloud-full-nosearch-20260614-211150 --run-id ibh-rustcloud-full-nosearch-20260614-211150 --packets /home/exedev/eval-runs/ibh-rustcloud-full-nosearch-20260614-211150/judge_packets.json --state-db /home/exedev/eval-runs/ibh-rustcloud-full-nosearch-20260614-211150/state/state.db --out-dir /tmp/ibh-jsonl-judge-fallback --expected-total 106 --run-label jsonl-fallback-smoke`
- `scripts/prepare-ibh-judge.py --run-root /home/exedev/eval-runs/ibh-browser-harness-supervisor-goal-tmux-gpt55-20260618-053222 --run-id ibh-browser-harness-supervisor-goal-tmux-gpt55-20260618-053222 --packets /home/exedev/eval-runs/ibh-browser-harness-supervisor-goal-tmux-gpt55-20260618-053222/judge_packets.json --state-db /home/exedev/eval-runs/ibh-browser-harness-supervisor-goal-tmux-gpt55-20260618-053222/state/state.db --out-dir /tmp/ibh-sqlite-judge-smoke --expected-total 106 --run-label sqlite-smoke`
- `scripts/aggregate-ibh-judgments.py /tmp/ibh-aggregate-smoke/judge --run-root /tmp/ibh-aggregate-smoke --run-id aggregate-smoke --expected-total 106`
- `scripts/finalize-ibh-judged-run.sh --run-root /tmp/ibh-aggregate-smoke --run-id aggregate-smoke --judge-dir /tmp/ibh-aggregate-smoke/judge --reference-aggregate /home/exedev/eval-runs/ibh-purecodex-175254-rejudge-jsonl-20260613/judge_aggregate.json --out /tmp/ibh-aggregate-smoke/comparison.md --expected-total 106`

Results:

- Dry-run command used `dataset-run-openai
  /home/exedev/datasets/Internal_Bench_hard.json --all --model gpt-5.5
  --max-turns 10000 --python-timeout-seconds 180 --max-attempts 1
  --concurrency 25 --browser-mode cloud`.
- Dry-run with `--judge` also emitted
  `scripts/judge-ibh-chunks-claude.py --judge-dir $ROOT/judge --run-root $ROOT
  --model sonnet --concurrency 5 --claude-bin claude`.
- `MAX_TURNS=80` dry-run exited before launch with
  `MAX_TURNS must stay >= 10000 for Internal_Bench_hard parity`.
- Non-dry wrapper preflight on this VM exits before launch and now reports both
  missing launch credentials:
  `missing OPENAI_API_KEY or LLM_BROWSER_OPENAI_API_KEY, and no stored OpenAI key
  from auth login openai` plus
  `missing BROWSER_USE_API_KEY, and no stored Browser Use Cloud key from auth
  login browser-use-cloud`.
- Rechecked non-dry wrapper preflight after the full verifier and local Chrome
  smoke; it still exits before launch with both missing credential messages, so
  a credentialed continuation still needs both model and cloud-browser auth
  before the benchmark can start.
- CLI no-80 tests: `2 passed`.
- Focused SDK JSON-RPC tests: `3 passed`.
- Provider default-auth fallback tests: `3 passed`.
- SDK/cloud default-auth fallback tests: `3 passed`.
- Focused simple-harness prompt/artifact-audit Rust tests: `4 passed`.
- Dataset runner/audit Rust tests: `13 passed`.
- Focused simple artifact-audit Python tests: `12 passed`.
- Focused locked judge-runner Python tests: `3 passed`.
- Focused completion-audit Python tests: `2 passed`.
- Focused Internal_Bench_hard wrapper Python tests: `7 passed`, including the
  regressions that missing OpenAI and Browser Use Cloud credentials are reported
  together before launch, and that `--judge` fails before launch when the Claude
  judge binary or raw-reference aggregate is missing.
- Focused judge-prep format tests: `2 passed`.
- Combined focused Python wrapper/judge/prep/audit tests: `26 passed`.
- Fake non-dry Internal_Bench_hard wrapper run: `106` synthetic tasks produced
  `106` judge packets, five chunk files sized `22/22/22/22/18`,
  `judge_aggregate.json` with `106/106`, and
  `current-vs-raw-judged-delta.md` against a fake reference aggregate. This
  proves the wrapper orchestration after the model/browser run completes; it is
  not a substitute for a credentialed cloud benchmark score.
- Python SDK/worker/judge/audit package tests: `58 passed, 1 skipped`.
- `cargo fmt --check`: passed.
- `scripts/verify-terminal-ui.sh`: passed. This included the full Rust test
  sweep, Python tests (`69 passed, 1 skipped`), deterministic Ratatui dumps, and
  the real tmux terminal smoke. Inspected `/tmp/but-design-loop` deterministic
  dumps (`empty`, `setup`, `account`, `model`, `done`, `running`, `cancelled`,
  `browser`, `history`, `developer`) plus tmux smoke captures for browser/model
  panels, history, bracketed paste, resize, pause/escape, follow-up, and
  completed-output flows.
- `scripts/verify-browser-harness-supervisor.sh`: passed. It verified the
  cloud-only simple-harness dry-run command (`--max-turns 10000`,
  `--concurrency 25`, `--browser-mode cloud`), rejected `MAX_TURNS=80`, ran
  Python eval tooling tests (`26 passed`), Rust dataset tests (`13 passed`),
  Rust simple-harness tests (`23 passed`), the isolated Codex missing-credentials
  test, and audited the previous judged `90/106` run root as complete.
- `scripts/verify-browser-harness-supervisor.sh --full`: passed. It repeated
  the supervisor/eval checks above and then ran `scripts/verify-terminal-ui.sh`;
  the nested terminal verifier passed with the full Rust test sweep, Python
  tests (`69 passed, 1 skipped`), deterministic Ratatui dumps, and real tmux
  smoke. Fresh artifacts were inspected in `/tmp/but-design-loop` at
  `2026-06-18 12:43-12:44 UTC`, including deterministic dumps for `empty`,
  `setup`, `account`, `model`, `done`, `running`, `cancelled`, `browser`,
  `history`, and `developer`, plus tmux captures for browser/model panels,
  history, bracketed paste, resize, pause/escape, follow-up, and completed
  output.
- `scripts/verify-browser-harness-supervisor.sh --live-browser`: passed. It
  repeated the supervisor/eval checks, then ran the isolated local Chrome/CDP
  smoke without a manual `CHROME_PATH`; `scripts/live-browser-boundary-smoke.sh`
  found `/opt/google/chrome/google-chrome` through the explicit
  `LLM_BROWSER_ALLOW_GOOGLE_CHROME=1` opt-in. Fresh state root:
  `/tmp/but-live-browser-boundary-supervisor-20260618-125042`; download task
  `4048ac87-0525-4e4e-b736-afd4e9a72c92`; stale recovery task
  `cbcaa31a-b6d3-4cdd-80b5-1de6da5973eb`.
- `scripts/verify-browser-harness-supervisor.sh --full --live-browser`:
  passed. This combined the full supervisor/eval verifier, full terminal UI
  verifier, and isolated local Chrome/CDP smoke in one run. It verified the
  cloud-only simple-harness dry-run command (`--max-turns 10000`,
  `--concurrency 25`, `--browser-mode cloud`), rejected `MAX_TURNS=80`, ran
  Python eval tooling tests (`24 passed` at the time of that run), Rust dataset tests (`13 passed`),
  Rust simple-harness tests (`23 passed`), the isolated Codex missing-credentials
  test, audited the previous judged `90/106` run root as complete, ran the full
  Rust/TUI/Python terminal verifier (`71 passed` Python tests inside the nested
  verifier), passed tmux smoke with fresh `/tmp/but-design-loop` captures at
  `2026-06-18 12:57 UTC`, and passed live browser smoke with state root
  `/tmp/but-live-browser-boundary-supervisor-20260618-125745`. The live smoke
  produced download task `6422c9b1-2160-44ff-b8d6-33008b869b21`, stale recovery
  task `baecf311-ebae-4aa8-9933-a1584693ffa5`, copied `report.csv` with
  `alpha,beta` / `1,2`, browser events, and capture frames.
- `scripts/live-browser-boundary-smoke.sh`: passed with
  `CHROME_PATH=/bin/google-chrome` and
  `LLM_BROWSER_LIVE_STATE_DIR=/tmp/but-live-browser-boundary-20260618-124701`.
  The script launched isolated headless Chrome with a temporary profile, drove
  the terminal browser-script command through a dedicated CDP port, copied a
  downloaded CSV artifact whose contents were `alpha,beta` / `1,2`, and verified
  stale target recovery preserved the selected tab. The state root contains
  browser events and capture frames for task ids
  `a0bb121b-bea2-45f8-80c0-72ef2886829e` and
  `d8c3ddc3-8678-49e3-911c-0b7176fd3cb0`.
- Replayed `prompts/simple-harness-artifact-audit.py` against the previous
  `90/106` run's failed artifacts. It now catches:
  - `8hyexf`: GoDaddy `Auction End Time` span `245.3` hours instead of the
    requested 24-hour filter.
  - `mly4ly`: nested contact email has `1/2` missing values despite required
    `management_email`.
  - `l3gywi`: `Title`, `VIN`, `miles`, and `price` are missing for every row.
  - `q85jsg`: when the marker-shaped `session.done` payload is mirrored into
    `result.txt`, audit flags the final's own admission that listing details,
    phone numbers, image URLs, or source access were unavailable.
  - `swebnv`: when the list-shaped blocked final is mirrored into `result.txt`,
    audit flags that Yelp/source blocking prevented reliable no-website
    business collection.
  - `h42m44`: empty result arrays.
  - `82kkzm`: one Google News article contains security-verification/403 text
    instead of complete article text.
  - `afeyuh`: review extraction is explicitly limited to visible/auth-unblocked
    reviews despite the complete-list requirement.
  - `y72ivg`: required eBay UK/USA marketplace coverage is explicitly missing
    or source-limited.
  - `jgzlma`: when the JSON-shaped `session.done` payload is mirrored into
    `result.json`, audit flags a likely non-supplement Galaxus product
    (`cleanser`), matching the judged source-scope miss.
  - `mvxpj4`: replay now passes the artifact audit after avoiding a false
    positive on the task-required `Deadline: null`; this task still needs judge
    evidence review rather than a simple artifact-shape rejection.
- TUI feedback surface test: `1 passed`.
- Python browser-harness admin/daemon/run tests: `76 passed`.
- JSONL fallback judge prep: `106` packets, `5` chunks, `106` event logs, no
  missing native sessions.
- SQLite judge prep: `106` packets, `5` chunks, `106` event logs, no missing
  native sessions.
- Locked judge-runner dry-run against the previous judged `90/106` run found
  the existing five chunks, validated row counts `22/22/22/22/18`, and skipped
  them without failures because `--overwrite` was not set.
- Judge aggregate/finalize smoke: `106` rows, `86/106` copied score, no
  validation errors, comparison written to `/tmp/ibh-aggregate-smoke/comparison.md`.
- Completion audit against the previous real judged `90/106` run initially
  failed because the run root lacked `current-vs-raw-judged-delta.md`; after
  running `scripts/finalize-ibh-judged-run.sh` on that run, the audit passed
  with `106` judge packets, `106` native event logs, five judge chunks,
  aggregate `90/106`, and a run-local comparison file.

## Do Not Mark Complete Until

- A fresh subset or full `Internal_Bench_hard` run is launched with model and
  Browser Use cloud credentials.
- The run is judged with the locked strict rubric from generated packets and
  native event traces.
- `scripts/finalize-ibh-judged-run.sh --run-root "$ROOT" --run-id "$RUN_ID"`
  produces a task-by-task comparison against the raw reference aggregate.
- The judged score and regressions are recorded, with every miss tied to
  artifact/event evidence.
