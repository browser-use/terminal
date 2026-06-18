# Internal_Bench_hard: No-80 + Simple Harness Gap

Date: 2026-06-18

Current run:

- Run root: `/home/exedev/eval-runs/ibh-browser-harness-supervisor-goal-no80-openai-20260618-071302`
- Score: 90/106 strict judged, 84.9%
- Failed ids: `6dpbhs`, `82kkzm`, `8hyexf`, `afeyuh`, `az39pe`, `eo2t8f`, `h42m44`, `jgzlma`, `l3gywi`, `mly4ly`, `mvxpj4`, `pvs7hz`, `q85jsg`, `swebnv`, `togn1w`, `y72ivg`
- Confirmed no effective 80-turn cutoff in that run: at least `rlexdw` reached 278 turns and `6dpbhs` reached 103 turns.

Reference comparator:

- Run root: `/home/exedev/eval-runs/ibh-purecodex-175254`
- Rejudge: `/home/exedev/eval-runs/ibh-purecodex-175254-rejudge-jsonl-20260613/judge_aggregate.json`
- Score: 96/106 strict rejudged

## Active Cap Fix

The active 80-turn defaults were removed from runnable paths:

- `AgentRunOptions::default().max_turns` is now `10_000`.
- Dataset provider CLI defaults use `DATASET_PROVIDER_DEFAULT_MAX_TURNS = 10_000`.
- Direct provider dataset runs now reject `Internal_Bench_hard` when
  `--max-turns` is below `10_000`, so bypassing the helper script with the old
  `--max-turns 80` command fails before launching any tasks.
- The fake dataset runner config uses the same dataset default.
- The runtime phase doc command now uses `--max-turns 10000`.

Verification:

- Active-cap grep across `crates`, `python`, `prompts`, `docs`, and `scripts`
  returns no runnable hits for old 80-turn defaults, assertions, or command
  examples.
- `scripts/verify-browser-harness-supervisor.sh --full` passes. Its dry-run
  command uses `--max-turns 10000`, `--concurrency 25`, and
  `--browser-mode cloud`; its no-80 guard rejects `MAX_TURNS=80`; and its nested
  terminal verifier passes with fresh dumps and tmux captures in
  `/tmp/but-design-loop`.
- `scripts/verify-browser-harness-supervisor.sh --live-browser` passes. It
  repeats the same cloud/no-80/eval-tooling checks and adds the isolated local
  Chrome/CDP smoke without requiring a manual `CHROME_PATH` on this Linux VM.
- `scripts/verify-browser-harness-supervisor.sh --full --live-browser` passes.
  It is the strongest local preflight before a credentialed benchmark run: it
  combines the no-80/cloud/simple-harness eval checks, focused judge tooling
  tests, full terminal UI verifier, tmux smoke, previous judged-run completion
  audit, and isolated local Chrome/CDP smoke.
- `cargo fmt --check`
- `cargo test -q -p browser-use-agent agent_run_options_defaults_match_core -- --nocapture`
- `cargo test -q -p browser-use-agent provider_run_config_new_uses_explicit_source_and_default_options -- --nocapture`
- `cargo test -q -p browser-use-cli dataset -- --nocapture`
- `cargo test -q -p browser-use-cli internal_bench_hard_rejects_low_turn_caps -- --nocapture`
- `cargo test -q -p browser-use-cli internal_bench_hard_accepts_parity_turn_limit -- --nocapture`
- `MAX_TURNS=80 scripts/run-internal-bench-hard-openai.sh --dry-run ...`
  exits `2` with `MAX_TURNS must stay >= 10000`.

Historical docs still mention old 80-turn failures as forensic notes.

## Current-Only Misses vs 96 Reference

These are the tasks this implementation failed while the 96/106 reference passed:

| Task | Current failure | Reference behavior | Likely lever |
| --- | --- | --- | --- |
| `6dpbhs` | Wrong/weak historical clue chain; final answer not supported. | Found `Walter Allen` through source evidence. | Evidence discipline before finalizing an identity. |
| `82kkzm` | Top-5 Google News result includes an FT security page instead of complete article text. | Saved five records and audit notes for constrained article text. | Source-limited finalization: mark incomplete or replace blocked article only with grounded evidence. |
| `8hyexf` | GoDaddy CSV not limited to auctions ending within 24h. | Exported filtered auction CSV. | Hard-filter verification: sale type/time window must be checked on saved rows. |
| `afeyuh` | Amazon product/review extraction incomplete; picked broader/similar result. | Searched exact `plazzo`, selected highest-rated exact result, saved public review cards. | Exact-query and selection-metric discipline. |
| `jgzlma` | Galaxus section includes non-supplements. | Top supplement lists only. | Category/source-scope verification before final. |
| `l3gywi` | CSV has blank required VIN/miles/price/title fields. | Exact requested URLs preserved with `N/A` and source-unavailable evidence. | Required-field handling: no blanks; no substitutes. |
| `mly4ly` | One required management email blank. | Judged acceptable because source evidence showed no email exposed. | Required-field handling with explicit source limitation. |
| `mvxpj4` | Dice jobs include Spring TX/remote records labeled as Tallahassee. | Saved raw checks proving Tallahassee/on-site/last-3-days scope. | Hard-filter verification of location/job type/date. |
| `q85jsg` | Yad2 blocked; many required fields missing. | Extracted detailed listings with phone/images. | Site/proxy/source fallback; prompt alone may not fix. |
| `swebnv` | Yelp blocked; no qualifying businesses collected. | Used cache/detail evidence to produce no-website businesses. | Site fallback/search-cache strategy; prompt alone may not fix. |
| `y72ivg` | eBay UK/US exact-part comparisons incomplete. | Covered Amazon UK, JIB, Amazon US, eBay UK, and eBay US. | Required-source coverage verification before final. |

## Patch Applied

`prompts/dataset-case-simple-harness-user.md` now keeps the raw browser-harness interface but restores three task-discipline contracts:

- Long extraction: discover source/pagination/filter pattern first; checkpoint and verify count/schema/fields/source coverage.
- Hard filters: exact query terms, source names, locations, dates, categories, sale types, ranking order, and marketplaces are hard requirements.
- Required fields: no blank structured fields; use `N/A` or `unknown` only with source-limitation evidence, and never substitute similar records.

This is intentionally a prompt-only change. It does not reintroduce `browser_script`, `done(...)`, `audit_artifact`, or Rust-owned browser interaction into the simple/raw harness path.

Product wiring update:

- CLI run constructors default to `simple_harness=true`.
- TUI run constructors default to `simple_harness=true` while preserving selected
  browser backend, local profile id/label, local browser name, and cloud API key
  env propagation.
- SDK `agent.run` config defaults to `simple_harness=true` while preserving
  Browser Use browser options, browser profile id/label, local browser label,
  and provider/model selection.
- `simple_harness=false` remains an explicit config override for legacy/debug
  comparison runs.
- Runtime config overrides now materialize `browser_profile_id`,
  `browser_profile_label`, and `browser_local_browser`, so config-driven runs do
  not silently drop selected profile state.
- Dataset runs now have an explicit tested merge boundary: per-case dataset
  options keep browser mode, max turns, Python timeout, and Python/browser env,
  while provider options supply `simple_harness`, model provider id, config
  profile/overrides, approval settings, and MCP/child-agent wiring.
- Provider tool registration now has an explicit simple-harness boundary:
  `browser`, `browser_script`, `python`, `done`, local DuckDuckGo `search`,
  goal tools, and subagent tools are not model-visible when
  `simple_harness=true`; `shell`, `exec_command`, `write_stdin`, `view_image`,
  hosted `web_search`, and `tool_search` remain available.
- `tool_search` is tested through the real dispatcher so the searchable catalog
  cannot leak the hidden Rust browser/CDP tools back into the model.
- The `browser-harness` command shim is now tested against a fake real harness:
  it forwards stdin/argv and preserves stdout, stderr, and exit status exactly,
  including a nonzero exit code.
- Simple harness preparation now writes and starts a per-session
  `browser-harness-worker` process. The generated shim talks to that worker over
  `BU_HARNESS_WORKER_SOCKET`; the worker proxies to the real Python
  browser-harness with the same argv/stdin/env and returns raw stdout, stderr,
  and exit code. Cleanup shuts the worker down and records worker cleanup status.
- The worker now persists product/lifecycle events separately in
  `tmp/browser-harness-worker-events.jsonl`, including worker start/stop,
  ping/shutdown, request start/finish, and request error metadata. Tests assert
  these events do not appear in model-visible stdout/stderr.
- Simple harness env now forwards product browser/profile settings without
  changing the model-visible command surface: `BUT_BROWSER_MODE`,
  `BUT_BROWSER_PROFILE_ID`, `BUT_BROWSER_PROFILE_LABEL`, and
  `BUT_BROWSER_LOCAL_BROWSER`. In cloud mode it also maps selected profile state
  to browser-harness native autospawn env via `BU_AUTOSPAWN_PROFILE_ID`, or
  `BU_AUTOSPAWN_PROFILE_NAME` when only a profile label/name is available.
- The simple-harness runtime dir now includes a state-dir hash as well as the
  session id, so parallel tests/runs with the same session id in different state
  dirs do not collide on the same socket.

Follow-up implementation cleanup:

- Runtime agent execution now only prepares/mirrors/cleans the raw
  browser-harness filesystem when `simple_harness=true`. The default product
  path still uses raw browser-harness, but explicit `simple_harness=false`
  comparison runs no longer get hidden harness shims or final-file mirroring.
- History rows now have deterministic newest-first ordering when sessions share
  the same millisecond timestamp. SQLite, the TUI cache, and protocol projection
  all break `updated_ms` ties with `created_ms` before `id`. This fixed the
  flaky real-terminal smoke path that repeatedly switched between a running
  transient task and a completed long transcript.
- TUI `/browser` and `/profile` wiring was re-audited against focused tests:
  browser backend selection persists local/cloud/managed choices, default
  profile selection persists stable local profile ids/labels, existing sessions
  keep their browser/profile snapshot, and new tasks use the latest default.
- SDK JSON-RPC now has explicit `browser.settings`, `browser.set_backend`, and
  `browser.set_profile` methods. These persist the same Rust browser/profile
  settings used by the TUI, and SDK `agent.run` / `agent.run_task` consume those
  settings as fallback browser defaults without leaking them into explicit
  `browser_id` or remote-CDP runs.
- The Python SDK source package has been restored under `python/browser_use`.
  `Client.browser.set_backend`, `Client.browser.set_profile`, and
  `Client.agent.run` are thin wrappers over the same Rust JSON-RPC methods, so
  the packaged SDK no longer depends on stale `__pycache__` artifacts.
- Simple-harness dataset prompts now tell the model to run the existing
  `artifact-audit` shell command before finalizing structured result files.
  This keeps the raw browser-harness surface but gives the model a cheap
  self-check for the failure classes seen in the `90/106` run.
- `artifact-audit` now catches nested required management emails, explicit
  "Hours to end 24" auction CSVs, blocked article text, visible-only review
  extraction for complete-review tasks, missing required eBay marketplace
  coverage, underfilled top-20 platform arrays, likely non-supplement products
  in dietary-supplement tasks, and basic Dice row scope for location/on-site/date
  filters. Replay against previous failed artifacts flags `8hyexf`, `mly4ly`,
  `l3gywi`, `q85jsg`, `h42m44`, `82kkzm`, `afeyuh`, `y72ivg`, and mirrored
  `jgzlma`.
- Dataset runner result preservation now mirrors valid JSON-shaped final text
  into `cwd/result.json` when the task asks for JSON/schema output and the model
  did not save a canonical result file. This directly addresses `jgzlma`, where
  the previous run had a large `session.done` JSON payload but no durable
  `result.json` for the judge.
- Dataset runner result preservation also mirrors explicit list/table/marker
  final text into `cwd/result.txt` when the task asks for that text format and
  the model did not save a canonical result file. This makes marker-style finals
  such as `q85jsg` judge/audit-visible without fabricating a structured file.
- `artifact-audit` now rejects finals that explicitly admit source blocking or
  missing required details for no-website business collection and
  listing-marker extraction. Replaying the previous `90/106` run now flags
  `swebnv` as blocked/incomplete and `q85jsg` as missing listing/source
  evidence instead of letting those final messages look like completed work.
- SDK cloud runs now pass a stored Browser Use Cloud key into the harness env,
  so SDK behavior matches terminal-authenticated TUI behavior.
- Fresh benchmark state now uses product-global auth consistently: provider
  resolution reads env, the current run store, then the default terminal auth
  store for `auth.*.api_key`; Browser Use Cloud harness env injection does the
  same for `auth.browser_use_cloud.api_key`; and
  `scripts/run-internal-bench-hard-openai.sh` accepts either env credentials or
  stored `auth status` credentials before launching.
- Command parity now has an explicit raw-vs-supervised fixture:
  `browser_harness_worker_matches_direct_harness_output_for_fixed_trace` runs a
  direct fake `browser-harness` command and the generated shim+worker path with
  identical argv/stdin, then asserts stdout, stderr, and exit code are
  byte-identical while worker events stay out of model-visible streams.
- Product `/domains` policy now reaches simple-harness browser work without a
  Rust browser tool. `prepare_existing_session` maps store allow/deny lists into
  `BU_BROWSER_ALLOWED_DOMAINS` / `BU_BROWSER_PROHIBITED_DOMAINS`, records the
  applied policy in `harness.prepared`, and generated
  `BH_AGENT_WORKSPACE/agent_helpers.py` exposes `nav_policy()` plus guarded
  `new_tab`, `goto_url`, and `http_get`.
- Simple-harness security prompt context no longer references hidden
  `browser_script` helpers; it only describes raw browser-harness domain-policy
  and email helpers.
- Raw browser-harness now has `/email` product service access without changing
  browser/CDP ownership. Generated `BH_AGENT_WORKSPACE/agent_helpers.py`
  exposes `current_datetime()`, `email_address()`, `email_inbox()`, and
  `email_message(message_id)`, backed by
  `browser-use-terminal --state-dir ... secrets email ...`. The harness env
  carries `BUT_STATE_DIR` and, when discoverable, `BUT_BROWSER_USE_TERMINAL_BIN`
  so the helper targets the current product store.
- Saved credentials and imported passwords now reach raw browser-harness through
  a redacted worker bridge. Generated helpers expose `available_secrets()`,
  placeholder-only `secret()` / `totp()`, and secret substitution inside
  `type_text()` / `fill_input()`. Secret metadata is passed as
  `BU_BROWSER_SECRET_META`; real values are resolved through the per-session
  worker socket and kept in worker memory so command stdout/stderr can be
  redacted before Codex sees it.
- The hidden `secrets harness-secret` CLI resolver refuses direct shell use
  unless `BU_HARNESS_WORKER_ACTIVE=1`, and the worker sets that only for its
  internal resolver subprocess.
- `/domains` allow-lists now union saved-secret primary and allowed domains
  before reaching raw harness env, so enabling an allow-list does not block the
  login/SSO domains required by configured credentials.
- Supervised runs now honor `BROWSER_HARNESS_SRC` for every delegated
  browser-harness invocation, including the per-session worker path. This keeps
  local experiments pinned to `/home/exedev/repos/browser-harness/src` instead
  of silently using the installed uv tool package.
- Forced-cloud wrapper bootstrap now forwards selected cloud profile state into
  browser-harness cloud creation: `BU_AUTOSPAWN_PROFILE_ID` becomes
  `profileId`, otherwise `BU_AUTOSPAWN_PROFILE_NAME` becomes `profileName`.
  Without this, the Rust shim could start a clean cloud browser before raw
  browser-harness saw the selected profile env.
- Forced-cloud wrapper reuse now requires a healthy CDP probe from an existing
  remote daemon. If the daemon is alive and its log says remote but
  `Target.getTargets` fails, the shim restarts it and provisions a fresh cloud
  browser before running the raw harness command.
- Selected local Chrome profile attach now lives in the Python browser-harness
  layer rather than Rust CDP code. For local runs with `BUT_BROWSER_PROFILE_ID`,
  the harness opens a marker URL in the selected profile, passes
  `BU_LOCAL_PROFILE_TARGET_MARKER` into the daemon, waits for that target,
  captures its `browserContextId`, and refuses arbitrary attach if the marker
  target does not appear.
- Product-local mode now blocks before daemon startup when
  `BUT_BROWSER_MODE=local` has no `BUT_BROWSER_PROFILE_ID`, preventing Local
  Chrome from attaching to an arbitrary available profile. This guard is scoped
  to the terminal product env; raw browser-harness usage without
  `BUT_BROWSER_MODE=local` still behaves normally.
- Warm local daemon reuse now requires `local_profile_verified=true` for the
  selected profile before reuse; otherwise the daemon restarts and goes through
  marker attach again. Remote/cloud daemon envs skip this local-profile check so
  cloud profile ids do not affect Internal_Bench_hard cloud runs.

Latest local verification:

- `cargo test -q -p browser-use-agent live_executor::tests::run_blocking_prepares_simple_harness_and_mirrors_final_answer -- --nocapture`
- `cargo test -q -p browser-use-agent live_executor::tests::run_blocking_skips_simple_harness_when_disabled -- --nocapture`
- `cargo test -q -p browser-use-agent simple_harness::tests::max_turns_still_honors_run_config_in_simple_harness -- --nocapture`
- `cargo test -q -p browser-use-agent browser_harness_shim_preserves_stdout_stderr_exit_and_stdin -- --nocapture`
- `cargo test -q -p browser-use-agent browser_harness_shim_exports_source_override_to_real_harness -- --nocapture`
- `cargo test -q -p browser-use-agent browser_harness_cloud_bootstrap_forwards_selected_profile_id -- --nocapture`
- `cargo test -q -p browser-use-agent browser_harness_cloud_bootstrap_restarts_stale_remote_daemon -- --nocapture`
- `cargo test -q -p browser-use-agent browser_harness_shim_uses_supervised_worker_when_available -- --nocapture`
- `cargo test -q -p browser-use-agent browser_harness_worker_matches_direct_harness_output_for_fixed_trace -- --nocapture`
- `cargo test -q -p browser-use-agent prepare_existing_session_applies_domain_policy_to_harness_env -- --nocapture`
- `cargo test -q -p browser-use-agent browser_harness_prompt_context_only_mentions_raw_harness_helpers -- --nocapture`
- `cargo test -q -p browser-use-agent browser_harness_prompt_context_mentions_email_helpers_when_configured -- --nocapture`
- `cargo test -q -p browser-use-agent browser_harness_secret -- --nocapture`
- `cargo test -q -p browser-use-agent browser_harness_worker_redacts_resolved_secret_output -- --nocapture`
- `cargo test -q -p browser-use-agent prepare_existing_session_applies_secret_metadata_and_allowlist_union -- --nocapture`
- `cargo test -q -p browser-use-agent simple_harness_uses_codex_like_tool_surface -- --nocapture`
- `cargo test -q -p browser-use-agent simple_harness_tool_search_does_not_leak_hidden_tools -- --nocapture`
- `cargo test -q -p browser-use-agent simple_harness_env_is_applied_to_exec_command -- --nocapture`
- `cargo test -q -p browser-use-agent simple_harness -- --nocapture`
- `cargo test -q -p browser-use-agent runtime_config_overrides_materialize_max_turns_and_browser_mode -- --nocapture`
- `cargo test -q -p browser-use-agent`
- `cargo test -q -p browser-use-cli dataset_provider_merge_preserves_dataset_browser_limits_and_provider_surface -- --nocapture`
- `cargo test -q -p browser-use-cli sdk_json_rpc_browser_settings_feed_stored_defaults_to_run_config -- --nocapture`
- `cargo test -q -p browser-use-cli sdk_json_rpc_agent_run_task_executes_fake_backend_with_normalized_history -- --nocapture`
- `cargo test -q -p browser-use-cli sdk_json_rpc_agent_run_executes_fake_backend -- --nocapture`
- `cargo test -q -p browser-use-cli simple_harness_dataset_prompt_matches_shell_browser_harness_surface -- --nocapture`
- `cargo test -q -p browser-use-cli simple_harness_dataset_session_persists_simple_prompt -- --nocapture`
- `cargo test -q -p browser-use-cli dataset_artifact_audit -- --nocapture`
- `uv run --with pytest python -m pytest -q python/tests/test_simple_harness_artifact_audit.py`
- `uv run --with pytest python -m pytest -q python/tests/test_audit_ibh_run_completion.py`
- `uv run --with pytest python -m pytest -q python/tests/test_prepare_ibh_judge.py`
- `uv run --with pytest python -m pytest -q python/tests/test_judge_ibh_chunks_claude.py`
- `uv run --with pytest python -m pytest -q python/tests/test_internal_bench_wrapper.py`
- `uv run --with pytest python -m pytest -q python/tests/test_internal_bench_wrapper.py::test_internal_bench_wrapper_can_run_prepare_judge_and_finalize_with_fakes`
- `uv run --with pytest python -m pytest -q python/tests`
- `scripts/verify-browser-harness-supervisor.sh`
- `cargo fmt --check`
- `python3 -m py_compile scripts/audit-ibh-run-completion.py scripts/judge-ibh-chunks-claude.py scripts/prepare-ibh-judge.py scripts/aggregate-ibh-judgments.py scripts/compare-judged-runs.py`
- `scripts/audit-ibh-run-completion.py --run-root /home/exedev/eval-runs/ibh-browser-harness-supervisor-goal-no80-openai-20260618-071302 --run-id ibh-browser-harness-supervisor-goal-no80-openai-20260618-071302 --require-judged`
- `scripts/run-internal-bench-hard-openai.sh --dry-run --judge --run-id judge-dry-run --root /tmp/but-judge-dry-run`
- `scripts/judge-ibh-chunks-claude.py --judge-dir /home/exedev/eval-runs/ibh-browser-harness-supervisor-goal-no80-openai-20260618-071302/judge --run-root /home/exedev/eval-runs/ibh-browser-harness-supervisor-goal-no80-openai-20260618-071302 --dry-run`
- `scripts/prepare-ibh-judge.py ... --out-dir /tmp/ibh-jsonl-judge-fallback --expected-total 106`
- `scripts/prepare-ibh-judge.py ... --out-dir /tmp/ibh-sqlite-judge-smoke --expected-total 106`
- `scripts/finalize-ibh-judged-run.sh --run-root /tmp/ibh-aggregate-smoke --run-id aggregate-smoke --judge-dir /tmp/ibh-aggregate-smoke/judge --reference-aggregate /home/exedev/eval-runs/ibh-purecodex-175254-rejudge-jsonl-20260613/judge_aggregate.json --out /tmp/ibh-aggregate-smoke/comparison.md --expected-total 106`
- `cargo test -q -p browser-use-cli sdk_provider_run_config_passes_stored_cloud_key_to_harness_env -- --nocapture`
- `cargo test -q -p browser-use-cli sdk_provider_run_config_maps_browser_use_options_to_rust_core -- --nocapture`
- `cargo test -q -p browser-use-cli sdk_json_rpc_cloud_browser_requires_cloud_credentials_before_run -- --nocapture`
- `cargo test -q -p browser-use-cli secrets_email_commands_accept_inbox_and_message -- --nocapture`
- `cargo test -q -p browser-use-cli secrets_harness_secret_command_accepts_domain_and_name -- --nocapture`
- `cargo test -q -p browser-use-cli secrets_harness_secret_command_requires_worker_env -- --nocapture`
- `cargo test -q -p browser-use-cli`
- `cargo test -q -p browser-use-tui selected_session_keeps_browser_profile_while_new_task_uses_latest_default -- --nocapture`
- `cargo test -q -p browser-use-tui browser_select_can_choose_detected_local_browser -- --nocapture`
- `cargo test -q -p browser-use-tui browser_select_starts_on_current_and_labels_cloud_recommended -- --nocapture`
- `cargo test -q -p browser-use-tui browser_select_infers_current_local_browser_from_profile_id -- --nocapture`
- `cargo test -q -p browser-use-tui local_chrome_options_carry_session_profile_snapshot -- --nocapture`
- `cargo test -q -p browser-use-tui expanded_chromium_parent_selects_local_chromium -- --nocapture`
- `cargo test -q -p browser-use-tui browser_select_can_choose_chromium_managed_modes_without_notice -- --nocapture`
- `cargo test -q -p browser-use-tui switching_to_cloud_without_key_records_backend_change_before_auth -- --nocapture`
- `cargo test -q -p browser-use-store list_sessions_breaks_updated_ms_ties_by_created_ms_desc -- --nocapture`
- `cargo test -q -p browser-use-protocol history_ties_on_updated_ms_use_created_ms_newest_first -- --nocapture`
- `cargo test -q -p browser-use-tui history_selection_uses_projected_root_task_rows -- --nocapture`
- `scripts/tui-terminal-smoke.py`
- `scripts/verify-terminal-ui.sh`
- `scripts/verify-terminal-ui.sh` rerun in this worktree after the latest
  simple-harness/browser-harness changes. It passed cargo fmt, Rust tests,
  Python tests, deterministic Ratatui dumps, and the real tmux terminal smoke.
  Inspected `/tmp/but-design-loop/`, including browser/history dumps plus
  `tui-terminal-smoke-browser-panel.txt` and
  `tui-terminal-smoke-completed-output.txt`.
- `python3 -m py_compile /home/exedev/repos/browser-harness/src/browser_harness/admin.py /home/exedev/repos/browser-harness/src/browser_harness/daemon.py /home/exedev/repos/browser-harness/src/browser_harness/run.py`
- `cd /home/exedev/repos/browser-harness && uv run --with pytest python -m pytest -q tests/unit/test_admin.py tests/unit/test_daemon_profile_context.py`
- `cd /home/exedev/repos/browser-harness && uv run --with pytest python -m pytest -q tests/unit/test_run.py tests/unit/test_admin.py tests/unit/test_daemon_profile_context.py`
- `cd /home/exedev/repos/browser-harness && uv run --with pytest python -m pytest -q tests/unit`

Current eval blocker:

- This shell does not currently have `OPENAI_API_KEY` or
  `LLM_BROWSER_OPENAI_API_KEY` exported.
- This shell does not currently have `BROWSER_USE_API_KEY` exported.
- Do not run the 106-task Internal_Bench_hard eval until both model and cloud
  browser credentials are present in the environment.
- Use `scripts/run-internal-bench-hard-openai.sh` for the next full run. It
  loads `$ENV_FILE` / repo `.env` when present, enforces cloud-only
  simple-harness execution with 10k turns, concurrency 25, a 30s health check,
  `judge_packets.json` extraction, and automatic `$ROOT/judge/` preparation.
  Packet extraction now collapses duplicate session records by task id before
  judging, then asserts the final packet count equals the benchmark task count.
- Add `--judge` to the same command when Claude Code judge credentials are
  available. That runs the five locked chunk judges, validates `chunk_*.json`,
  aggregates the score, and writes the current-vs-raw comparison. The wrapper
  now preflights the Claude judge binary and raw-reference aggregate before the
  106-task model run starts.

Runner validation after that fix:

- `bash -n scripts/run-internal-bench-hard-openai.sh`
- `scripts/run-internal-bench-hard-openai.sh --dry-run --run-id dry-run-no80-check --root /tmp/but-dry-run-no80-check`
  emitted `--max-turns 10000`, `--concurrency 25`, and `--browser-mode cloud`.
- `ENV_FILE=<temp file>` dry-run confirmed the helper loads env-file defaults
  before building the command, without requiring credentials or printing secret
  values.
- Non-dry preflight with no stored/env credentials exits before launch and
  reports both missing requirements together: OpenAI model auth and Browser Use
  Cloud browser auth. The focused wrapper regression suite now covers this.
- Non-dry `--judge` preflight also exits before launch if the Claude judge binary
  or raw-reference aggregate is missing. The focused wrapper regression suite
  covers both cases.
- `MAX_TURNS=80 scripts/run-internal-bench-hard-openai.sh --dry-run ...`
  still exits `2` with `MAX_TURNS must stay >= 10000 for Internal_Bench_hard
  parity`.
- Replaying the extraction logic against
  `/home/exedev/eval-runs/ibh-browser-harness-supervisor-goal-no80-openai-20260618-071302`
  produced `106` packets, `106` unique task ids, `106` runner-ok records, and no
  missing task/cwd/artifact fields. The old naive extractor produced `107`
  packets because `gatj9t` had a transient failed provider record followed by
  a successful final record.

Current-vs-reference comparison tool:

- `scripts/prepare-ibh-judge.py` enriches runner packets from native
  `state/state.db`, exports one event JSONL per task, attaches session and
  artifact evidence, writes `packets_all.json`, splits the standard
  `22/22/22/22/18` judge chunks, and writes `judge_prompt.md` plus per-chunk
  briefs. `scripts/run-internal-bench-hard-openai.sh` calls this automatically
  after a full manifest is available.
- `scripts/judge-ibh-chunks-claude.py` runs those prepared chunks with Claude
  Code print-mode judges. It launches one judge per `packets_*.json`, validates
  returned strict JSON rows against the packet task ids, and writes
  `chunk_*.json`. `scripts/run-internal-bench-hard-openai.sh --judge` calls it
  automatically after packet prep.
- `scripts/audit-ibh-run-completion.py` is the mechanical trust gate for run
  roots. It verifies runner manifest counts, judge packet counts, native event
  logs, packet chunks, judge chunks, aggregate totals, and the run-local
  current-vs-raw comparison. The wrapper calls it after packet prep and again
  after judged finalization with `--require-judged`.
- `scripts/verify-browser-harness-supervisor.sh` is the fast local verifier for
  the whole supervisor/eval-ready architecture. It does not run the live
  benchmark; it verifies cloud/simple-harness/10k command construction, the
  no-80 guard, fake 106-task judge orchestration, completion auditing, focused
  Python eval tooling, and focused Rust dataset/simple-harness tests. Use
  `--full` when you also want the repo-owned real-terminal TUI verifier.
- `scripts/aggregate-ibh-judgments.py` aggregates judge `chunk_*.json` files
  against `packets_*.json`, writes `judge_aggregate.json` and
  `judge_summary.md`, and fails on missing, duplicate, unexpected, non-binary,
  malformed, or incomplete judge rows.
- `scripts/compare-judged-runs.py` compares two `judge_aggregate.json` files
  task by task and emits a markdown report with regressions, improvements,
  shared failures, and the full task matrix.
- `scripts/finalize-ibh-judged-run.sh --run-root "$ROOT" --run-id "$RUN_ID"`
  is the post-judge shortcut: it validates/aggregates chunk judgments and writes
  `$ROOT/current-vs-raw-judged-delta.md` against the raw Codex + browser-harness
  reference aggregate.
- Existing evidence report:
  `docs/eval-journal/current-vs-raw-judged-delta-20260618.md`.
- That report shows the latest judged implementation at `90/106` against the
  raw Codex + browser-harness reference at `96/106`: `11` current-only
  regressions, `5` current-only improvements, and `5` shared failures.
- Prep validation against the existing current run produced `106` enriched
  packets, `106` unique task ids, five chunks sized `22/22/22/22/18`, `106`
  native event logs, and zero missing native session mappings.
- Finalizer validation against a copied judge directory reproduced `90/106`
  with zero aggregate validation errors and a comparison summary of `11`
  current-only regressions, `5` current-only improvements, and `5` shared
  failures.

Implementation audit update after goal continuation:

- Completion audit file:
  `docs/eval-journal/browser-harness-supervisor-completion-audit-20260618.md`.
- `docs/agent-design/browser-harness-supervisor-implementation-plan.md` now
  records `/sync-cookies` as a product-side profile sync flow rather than a
  model-visible raw-harness helper.
- The same plan splits the previous broad "local Chrome setup/recovery" item
  into concrete proven claims: selected-profile scoping / wrong-profile
  prevention, remote-debugging setup classification, and one remaining
  end-to-end manual local Chrome recovery smoke.
- The plan now also records `/task`, `/history`, `/model`, `/context`, `/goal`,
  and `/feedback` as product-side TUI/session flows, not browser-harness worker
  responsibilities.
- Focused verification:
  - `cargo test -q -p browser-use-cli sync_cookies -- --nocapture`
  - `cargo test -q -p browser-use-cli internal_bench_hard -- --nocapture`
  - `cargo test -q -p browser-use-tui sync_cookies -- --nocapture`
  - `cargo test -q -p browser-use-tui feedback_palette_action_opens_feedback_surface -- --nocapture`
  - `cargo test -q -p browser-use-tui command_palette_filters_and_exposes_only_product_actions -- --nocapture`
  - `cd /home/exedev/repos/browser-harness && uv run --with pytest python -m pytest -q tests/unit/test_admin.py tests/unit/test_daemon_profile_context.py tests/unit/test_run.py`
  - `scripts/run-internal-bench-hard-openai.sh --dry-run --run-id dry-run-goal-continuation-2 --root /tmp/but-dry-run-goal-continuation-2`
  - `scripts/verify-terminal-ui.sh`
- `scripts/verify-terminal-ui.sh` passed. It ran cargo fmt, Rust tests, Python
  tests, deterministic Ratatui dumps, and the real tmux terminal smoke. I
  inspected `/tmp/but-design-loop/`, including `browser.txt`, `history.txt`,
  `model.txt`, `tui-terminal-smoke-browser-panel.txt`, and
  `tui-terminal-smoke-completed-output.txt`.
- The dry-run emitted the intended full benchmark shape:
  `dataset-run-openai /home/exedev/datasets/Internal_Bench_hard.json --all
  --model gpt-5.5 --max-turns 10000 --python-timeout-seconds 180
  --max-attempts 1 --concurrency 25 --browser-mode cloud`.

Expected direct lift if the model follows it:

- High confidence affected: `8hyexf`, `afeyuh`, `jgzlma`, `l3gywi`, `mvxpj4`, `y72ivg`
- Medium confidence affected: `6dpbhs`, `82kkzm`, `mly4ly`
- Low confidence affected by this patch alone: `q85jsg`, `swebnv`

## Next Validation

Run one focused subset before another full 106:

```bash
export OPENAI_API_KEY=...
export BROWSER_USE_API_KEY=...
export BROWSER_HARNESS_SRC=/home/exedev/repos/browser-harness/src
export LLM_BROWSER_BROWSER_MODE=cloud
export LLM_BROWSER_AUTO_CHROME=0
export LLM_BROWSER_OPEN_CLOUD_LIVE_VIEW=0

STAMP=$(date -u +%Y%m%d-%H%M%S)
ROOT="/home/exedev/eval-runs/ibh-simple-harness-filter-field-probe-$STAMP"
RUN_ID="ibh-simple-harness-filter-field-probe-$STAMP"
mkdir -p "$ROOT"

stdbuf -oL -eL ./target/debug/browser-use-terminal \
  --state-dir "$ROOT/state" \
  dataset-run-openai /home/exedev/datasets/Internal_Bench_hard.json \
  --task-id 6dpbhs \
  --task-id 82kkzm \
  --task-id 8hyexf \
  --task-id afeyuh \
  --task-id jgzlma \
  --task-id l3gywi \
  --task-id mly4ly \
  --task-id mvxpj4 \
  --task-id y72ivg \
  --model gpt-5.5 \
  --max-turns 10000 \
  --python-timeout-seconds 180 \
  --max-attempts 1 \
  --concurrency 9 \
  --browser-mode cloud \
  --run-id "$RUN_ID" 2>&1 | tee "$ROOT/run.log"
```

Judge those nine tasks with the same strict rubric. If at least three current-only misses flip without new obvious regressions in artifacts, run the full 106 once.
