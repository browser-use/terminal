# Browser-Harness Supervisor Implementation Plan

## Decision

Use the proven Codex + browser-harness interaction model as the model-facing
contract. Rust owns the product, session lifecycle, TUI, SDK, persistence, and
process supervision. Python browser-harness owns CDP and browser interaction.

This track intentionally does not pursue Rust-owned CDP for model-visible
browsing. Local tests showed that moving CDP ownership into Rust can degrade
task quality. The acceptance target is behavioral parity with raw Codex +
browser-harness, not a Rust rewrite of browser-harness internals.

## Non-Negotiable Invariant

For every model-visible browser command, Codex must see output that is
indistinguishable from raw browser-harness.

Rust may start, stop, monitor, configure, log, and persist. Rust must not
summarize, normalize, truncate, reword, or reinterpret browser observations.

## Target Architecture

```text
TUI / Python SDK / CLI
  -> Rust runtime supervisor
       - settings and profiles
       - task/session store
       - worker lifecycle
       - events and artifacts
       - model/provider selection
       - SDK JSON-RPC
       - terminal UI commands

Codex session
  -> browser-harness skill
  -> per-session browser command shim
       - sends argv/stdin to Python browser-harness worker
       - prints exact stdout/stderr
       - exits with exact code

Python browser-harness worker
  -> owns CDP/browser connection
  -> owns page observation/action logic
  -> owns local/cloud/profile attach behavior
  -> returns raw browser-harness output
  -> emits product events separately
```

The hot path is:

```text
Codex -> browser shim -> Python browser-harness worker -> browser/CDP
      <- exact stdout/stderr/exit code <-
```

Rust is not in the hot browser-command path after worker startup unless needed
for cancellation, cleanup, or product event collection.

## Phase 0: Freeze The Model Interface

Goal: prevent accidental eval regressions before adding product features.

- Codex receives the browser-harness skill text.
- Codex sees exactly one browser command surface: `browser` /
  `browser-harness`.
- No Rust browser/CDP tool is exposed to the model.
- No product setup dump is injected into the model prompt.
- No hidden final/audit/completeness tool is inserted into the model loop.
- Full final-answer capture remains written to final artifacts.
- Per-session home, workspace, tmp, and artifact dirs are stable.
- CLI, TUI, SDK, and dataset provider runs default to this raw harness surface.
  `simple_harness=false` remains an explicit compatibility/debug override, but
  it is no longer the product default.

Exit criteria:

- Contract tests prove the skill/shim are materialized.
- Contract tests prove stdout/stderr/exit code preservation.
- Provider contract tests prove `simple_harness=true` does not expose
  `browser`, `browser_script`, `python`, `done`, local `search`, goal tools, or
  subagent tools to the model.
- Provider dispatch tests prove `tool_search` cannot reintroduce those hidden
  tools through the searchable catalog.
- A one-task smoke run shows the same command affordance as raw
  browser-harness.

Current evidence:

- `simple_harness::tests::prepare_creates_codex_style_paths_and_skill` proves
  per-session skill and command shims are materialized.
- `simple_harness::tests::browser_harness_shim_preserves_stdout_stderr_exit_and_stdin`
  proves the `browser-harness` shim forwards stdin/argv and preserves stdout,
  stderr, and nonzero exit code.
- `simple_harness_uses_codex_like_tool_surface` and
  `simple_harness_tool_search_does_not_leak_hidden_tools` prove the model-visible
  provider surface stays raw-harness-shaped.
- `browser_harness_shim_uses_supervised_worker_when_available` proves the
  generated `browser-harness` shim talks to a Rust-started per-session worker
  over the worker socket.
- `browser_harness_worker_matches_direct_harness_output_for_fixed_trace`
  compares direct raw `browser-harness` execution against the generated
  shim+worker path for the same argv/stdin and proves stdout, stderr, and exit
  code are byte-identical.
- `browser_harness_shim_preserves_stdout_stderr_exit_and_stdin` proves the
  direct fallback path still forwards stdin/argv and preserves stdout, stderr,
  and exit status.

## Phase 1: Python Browser-Harness Worker

Goal: make browser-harness a long-lived per-session browser owner without
changing what Codex sees.

Worker responsibilities:

- Own the browser/CDP connection.
- Load the existing browser-harness implementation.
- Accept commands over a local socket or stdio protocol.
- Return exact stdout, stderr, exit code, and artifact metadata.
- Emit product events to a separate JSONL stream/file.
- Shut down cleanly when Rust ends the task.

Current implementation:

- Rust writes a per-session `browser-harness-worker` Python script and starts it
  before the model turn.
- The worker listens on a Unix socket under the per-session runtime dir and
  proxies requests to the real `browser-harness` executable.
- The worker preserves model-visible stdout, stderr, stdin, argv, environment,
  and exit code.
- The worker writes product/lifecycle events to a separate
  `browser-harness-worker-events.jsonl` file under the task artifact `tmp/`
  directory. Events include worker start/stop, ping, shutdown, request start,
  request finish, and request error metadata.
- Cleanup sends a worker shutdown request, waits for the child process, and
  records worker cleanup status plus worker event path/count in
  `harness.cleaned`.

Command protocol:

```json
{"id":"1","argv":["observe"],"stdin":""}
```

Response:

```json
{"id":"1","exit_code":0,"stdout":"...","stderr":"","artifacts":[]}
```

Rules:

- Do not intermix product events with model-visible stdout.
- Do not convert browser-harness exceptions into generic errors.
- Do not add wrapper text.
- Preserve artifact paths.
- Browser-harness owns command-level timeouts unless explicitly cancelled.

## Phase 2: Browser Command Shim

Goal: make Codex talk directly to the Python worker with no Rust browser-state
translation.

Per Codex session, write:

```text
<session-home>/.local/bin/browser
<session-home>/.local/bin/browser-harness
```

Shim behavior:

- Read argv and stdin.
- Connect to the worker from env.
- Send one command request.
- Print response stdout to stdout.
- Print response stderr to stderr.
- Exit with response exit code.

Current implementation:

- The generated `browser-harness` shim discovers the real browser-harness binary
  on `PATH`, excluding its own shim directory.
- If `BU_HARNESS_WORKER_SOCKET` is set, it execs
  `browser-harness-worker-client`, which sends argv/stdin/env to the worker.
- If no worker socket is available, the client falls back to running the real
  browser-harness directly with the already-read stdin bytes, preserving the raw
  command contract.
- When `BROWSER_HARNESS_SRC` is set, the shim exports it at the front of
  `PYTHONPATH` for every delegated browser-harness invocation, including the
  worker path. This keeps supervised runs pinned to the checked-out
  browser-harness source instead of silently falling back to an installed uv
  tool package.
- In forced cloud mode, the shim pre-bootstraps Browser Use Cloud before
  delegating to raw browser-harness. That bootstrap now forwards selected cloud
  profile env exactly like browser-harness native autospawn:
  `BU_AUTOSPAWN_PROFILE_ID` becomes `profileId`, otherwise
  `BU_AUTOSPAWN_PROFILE_NAME` becomes `profileName`.
- Forced-cloud bootstrap also probes the live daemon with `Target.getTargets`
  before reuse. If a previous remote daemon is still alive but its CDP
  connection is stale, the shim restarts it and provisions a fresh cloud browser
  instead of letting raw browser-harness fall through to local Chrome recovery.

## Phase 3: Rust Supervisor

Goal: make Rust own lifecycle and product state while staying out of browser
reasoning.

Rust responsibilities:

- Create task/session dirs.
- Start Python worker before the model turn.
- Pass compact config through env or an init file.
- Persist worker events separately from command responses.
- Monitor process health.
- Clean up worker on task finish/cancel.
- Expose worker status to TUI/SDK.
- Keep state in SQLite/events.

Rust must not:

- Open CDP directly for model-visible browsing.
- Summarize browser state.
- Rewrite command output.
- Choose fallback tabs invisibly.
- Add shorter browser-command timeouts than raw harness.

Current implementation:

- `AgentRunOptions` carries the selected browser backend plus optional product
  profile id, profile label, and local browser label.
- Simple harness preparation forwards that compact config to the command env as
  `BUT_BROWSER_MODE`, `BUT_BROWSER_PROFILE_ID`,
  `BUT_BROWSER_PROFILE_LABEL`, and `BUT_BROWSER_LOCAL_BROWSER`.
- Cloud mode additionally maps the selected profile to browser-harness native
  autospawn env: `BU_AUTOSPAWN_PROFILE_ID` when an id is present, otherwise
  `BU_AUTOSPAWN_PROFILE_NAME` from the label.
- The forced-cloud shell shim is tested with a fake browser-harness package to
  prove it forwards selected cloud profile id into `start_remote_daemon()`
  before the raw harness command runs.
- The same shell-shim fixture now covers stale remote daemon recovery: a daemon
  whose log says remote but whose CDP probe fails is restarted and replaced
  before the raw harness command runs.
- `harness.prepared` records the compact `browser_config` for debugging without
  injecting it into the model prompt.
- `harness.prepared` also records the worker event JSONL path. The worker event
  stream is persisted separately from model-visible command stdout/stderr.

## Phase 4: Product Browser Configuration

Goal: restore the previous terminal browser/profile UX as product config, not
model-visible complexity.

Commands/features to wire:

- `/browser`: select local, managed, cloud, or remote backend.
- `/profile`: select default profile; new sessions snapshot it.
- Existing chats keep the browser/profile snapshot they started with.
- `/sync-cookies`: sync selected local/cloud profile through product service.
- `/secrets`: configure credential scope.
- `/import-passwords`: import credential material into the product store.
- `/domains`: configure allow/block policy.
- `/email`: allocate/configure inbox.
- `/history`: read persisted sessions/events/final artifacts.
- `/task`: create a new session with a browser config snapshot.
- `/model`: configure provider/model.
- `/context`: inspect attribution/runtime debug state.
- `/feedback`: product feedback only.
- `/goal`: use native Codex goal support; do not duplicate it first.

Task-start config passed to Python worker should be compact:

```json
{
  "backend": "cloud",
  "profile_id": "google-chrome:Default",
  "state_dir": "...",
  "artifact_dir": "...",
  "domain_policy_id": "...",
  "secrets_scope": "...",
  "email_inbox_id": "..."
}
```

The model should not receive this config except when setup is blocked and a
clear user-facing instruction is needed.

Current implementation:

- TUI task construction already snapshots selected browser backend, local
  profile id/label, local browser label, and cloud API key into
  `AgentRunOptions`.
- `/browser` is wired through the TUI browser selection surface, persists
  backend/local-browser settings, records backend-change events, and restores
  stale live-browser state when the backend changes.
- `/profile` is wired through the TUI default-profile surface, persists stable
  local profile ids/labels, and existing sessions retain the profile snapshot
  they started with while new tasks use the latest default.
- Runtime config overrides now materialize `browser_profile_id`,
  `browser_profile_label`, and `browser_local_browser`, so config-driven runs do
  not silently drop them.
- SDK `agent.run` now maps browser `profile_id`, `profile`/`profile_label`, and
  `local_browser` into the same Rust options.
- SDK JSON-RPC now exposes `browser.settings`, `browser.set_backend`, and
  `browser.set_profile`. These methods write the same Rust store settings used
  by the TUI, and `agent.run` / `agent.run_task` use them as fallback browser
  defaults when the caller does not pass an explicit browser object or
  `browser_id`.
- The Python SDK package under `python/browser_use` exposes the same surface:
  `Client.browser.set_backend`, `Client.browser.set_profile`, and
  `Client.agent.run` call Rust JSON-RPC over stdio. The Python package does not
  own CDP and does not call browser-harness directly.
- SDK cloud runs propagate a Browser Use Cloud key from the terminal store into
  the harness environment, matching the TUI path instead of requiring the key to
  already be exported in the process environment.
- `/sync-cookies` remains a product service rather than a model-visible helper:
  the CLI accepts selected local profiles without conflicting with global
  profile settings, and the TUI `/sync-cookies` surface covers auth gating,
  local/cloud profile labeling, syncing state, and completion rendering.
- `/task` and `/history` remain product/session-store flows: `NewTask` clears
  the selected session and restores default runtime settings, while
  `OpenHistory` / `SelectHistory` browse persisted root task rows and reapply
  the selected session's runtime settings.
- `/model` remains product-side provider/model selection. The TUI opens the
  provider-first model surface, supports provider auth submenus, API-key and
  Codex-OAuth flows, searchable model lists, and session-scoped model
  selection.
- `/context` remains a product-only attribution surface over persisted model
  request, token, tool, and cache events.
- `/goal` is wired to the native Codex-style goal store and slash-command flow;
  this is intentionally not duplicated in the browser-harness worker.
- `/feedback` remains product-only. It opens from the command palette into the
  feedback questionnaire without adding any model-visible tool or browser
  behavior.
- `/domains` allow/deny settings are now applied to simple-harness sessions as
  raw browser-harness-compatible env (`BU_BROWSER_ALLOWED_DOMAINS` and
  `BU_BROWSER_PROHIBITED_DOMAINS`) and as a generated
  `BH_AGENT_WORKSPACE/agent_helpers.py`. The helper exposes `nav_policy()` and
  guards `new_tab`, `goto_url`, and `http_get` without introducing a Rust
  browser tool into the model-visible surface.
- `/email` is now available to raw browser-harness through the generated
  `BH_AGENT_WORKSPACE/agent_helpers.py`. The helper exposes
  `current_datetime()`, `email_address()`, `email_inbox()`, and
  `email_message(message_id)`, and routes those calls through
  `browser-use-terminal --state-dir ... secrets email ...` so the existing
  AgentMail-backed product store remains the source of truth.
- `/secrets` and `/import-passwords` now feed raw browser-harness through a
  redacted worker bridge. Rust passes metadata only in `BU_BROWSER_SECRET_META`.
  Generated helpers expose `available_secrets()`, `secret()`, `totp()`, and
  placeholder substitution in `type_text()` / `fill_input()`. Actual values are
  resolved through the per-session worker socket, kept in worker memory for
  stdout/stderr redaction, and never added to the model-visible prompt.
- When a `/domains` allow-list exists, simple-harness now folds saved-secret
  primary and allowed domains into `BU_BROWSER_ALLOWED_DOMAINS`, matching the
  older browser-script behavior that kept login/SSO domains reachable.
- In simple-harness mode, product security prompt context is limited to
  raw-harness-compatible credential, domain, and email guidance. The older
  saved-credential prompt that references `browser_script` remains only for the
  non-simple Rust browser-script path.
- Simple harness env propagation is tested for local profile selection, cloud
  profile-id autospawn, and cloud profile-name fallback.

## Phase 5: Previous Browser Setup Behavior

Goal: regain polished setup/recovery without moving CDP ownership to Rust.

Port behavior into the Python worker/browser-harness attach layer, with Rust
providing settings and UI state:

- No arbitrary local Chrome profile attach.
- If no default profile is selected, block before browser work.
- Stable profile ids such as `google-chrome:Default`,
  `google-chrome:Profile 1`, `brave:Default`, `chromium:Default`.
- Wrong-profile attach is blocked.
- Marker tabs can target selected local profile when needed.
- Browser work stays scoped to selected profile/browser context.
- Reconnect verifies selected profile.
- Separate setup states:
  - Chrome closed or stale port
  - remote debugging checkbox off
  - per-session permission popup/HTTP 403
  - selected profile target missing
  - previous tab target gone
- Internal Chrome targets are avoided.
- Closed tabs/windows are explicit recovery states.
- Local connect is single-flight with deadlines.
- CDP eval timeout does not drop a healthy browser connection.
- Cloud/local switching clears stale state.

Current implementation:

- The Python browser-harness attach layer already waits for
  `BU_LOCAL_PROFILE_TARGET_MARKER`, captures the marker target's
  `browserContextId`, filters `Target.getTargets`, injects the selected
  context into `Target.createTarget`, rejects cross-context target attach, and
  reports `profile-target-missing` / `target-gone` instead of drifting to an
  arbitrary tab.
- The Python harness supervisor now prepares selected local profile startup:
  when local mode has `BUT_BROWSER_PROFILE_ID`, it opens a marker URL in that
  selected profile and starts the daemon with
  `BU_LOCAL_PROFILE_TARGET_MARKER`.
- The browser-harness entrypoint now blocks product-local mode before daemon
  startup when `BUT_BROWSER_MODE=local` but no `BUT_BROWSER_PROFILE_ID` is set.
  This preserves normal raw browser-harness usage, but prevents the terminal
  product from attaching to an arbitrary available Chrome profile after the user
  selected Local Chrome without choosing `/profile`.
- Warm local daemon reuse now verifies that the daemon was marker-attached to
  the selected profile before reusing it; otherwise it restarts and goes
  through marker attach again.
- Remote/cloud daemon envs skip local profile verification, so cloud profile ids
  do not accidentally trigger local Chrome profile checks.
- The daemon reports `local_profile_verified` in `current_tab` and
  `connection_status`, giving the supervisor a concrete proof that selected
  local profile attach happened through the marker flow.
- Chrome remote-debugging setup errors are classified in the Python harness
  attach layer: HTTP 403 / handshake timeout maps to the per-session permission
  popup, and `DevToolsActivePort` / `enable chrome://inspect` maps to the
  checkbox-off setup flow. The recovery message includes the selected profile
  when one is configured.
- The selected-profile recovery path opens `chrome://inspect/#remote-debugging`
  through the selected Chromium profile command when possible, instead of
  asking the user to repair an arbitrary browser window.

## Phase 6: SDK Parity

Goal: make the SDK use the same runtime as the TUI.

SDK should call Rust runtime APIs:

```python
client.browser.set_backend("cloud")
client.browser.set_profile("google-chrome:Default")
result = client.agent.run("Find the price...")
```

SDK must not own CDP or call browser-harness directly.
SDK-created agent runs use `simple_harness=true` by default so the model sees
the same browser-harness command surface as TUI/CLI runs. Advanced callers may
pass `config_overrides={"simple_harness": false}` only for legacy debugging.

Current implementation:

- The Rust JSON-RPC server advertises `browser.settings`,
  `browser.set_backend`, and `browser.set_profile`.
- The Python `browser_use.Client` facade forwards browser settings and one-shot
  task runs to the Rust JSON-RPC server, while the browser-use-compatible
  `Agent(...).run()` path continues to use `browser.create`, `agent.create`,
  and `agent.run`.
- `browser.set_backend` accepts local, cloud, managed-headed,
  managed-headless, and remote-cdp aliases, then persists the normalized backend
  in the same app settings as the TUI.
- `browser.set_profile` accepts stable profile ids such as
  `brave:Default` or `google-chrome:Profile 1`, persists the id/label, and
  infers the local browser family from known id prefixes when needed.
- `agent.run` and `agent.run_task` read stored browser defaults only when no
  explicit browser object or `browser_id` should take precedence.

## Phase 7: Evaluation Gates

Gate A: command parity

- Compare raw browser-harness vs shimmed worker on fixed traces.
- Require stdout/stderr/exit-code preservation.

Gate B: simple eval smoke

- Run a small Internal_Bench_hard subset that previously exposed browser
  observation/action failures.

Gate C: product wiring smoke

- Run profile/backend-sensitive manual and automated flows.
- Run TUI smoke if TUI code changed: `scripts/verify-terminal-ui.sh`.

Gate D: full benchmark

- Run all 106 Internal_Bench_hard tasks with cloud browser mode.
- Judge strictly with complete artifacts/transcripts.
- Compare task-by-task against raw Codex + browser-harness reference.

## Implementation Completeness Checklist

- [x] Model-visible contract frozen and tested.
- [x] Python browser-harness command worker implemented.
- [x] Browser/browser-harness shims call worker directly.
- [x] Rust supervisor starts/stops worker per task.
- [x] Compact browser/profile config reaches simple-harness env and events.
- [x] SDK run config maps browser profile/local-browser fields to Rust options.
- [x] TUI task construction snapshots browser/profile fields into Rust options.
- [x] Worker events are persisted without polluting model-visible stdout.
- [x] TUI `/browser` wired.
- [x] TUI `/profile` wired.
- [x] SDK explicit backend/profile management APIs wired to Rust runtime.
- [x] Forced-cloud wrapper bootstrap forwards selected cloud profile id/name to
      browser-harness cloud creation.
- [x] Forced-cloud wrapper restarts stale remote daemons before falling through
      to raw browser-harness.
- [x] Product-local mode blocks browser-harness startup when no default local
      profile is selected.
- [x] Selected local Chrome profile marker attach/reuse restored in the Python
      browser-harness attach layer.
- [x] Previous local Chrome profile scoping and wrong-profile prevention
      restored in the Python browser-harness attach layer.
- [x] Previous local Chrome remote-debugging setup classification restored in
      the Python browser-harness attach layer.
- [ ] End-to-end manual local Chrome recovery smoke run in a real browser
      environment.
- [x] `/sync-cookies` remains wired as a product-side profile sync flow.
- [x] `/task`, `/history`, `/model`, `/context`, `/goal`, and `/feedback`
      remain wired as product-side TUI/session flows.
- [x] `/domains` integrated as a raw-harness-compatible product service.
- [x] `/email` integrated as a raw-harness-compatible product service.
- [x] `/secrets` and `/import-passwords` integrated with a redacted raw-harness
      bridge.
- [x] Command parity tests pass.
- [x] TUI verification passes for touched TUI flows.
- [ ] Internal_Bench_hard subset judged.
- [ ] Internal_Bench_hard full 106 judged.
- [ ] Task-by-task failure comparison report written.

## What Would Count As Failure

- Codex sees Rust-formatted browser observations.
- Browser command output is clipped or wrapped.
- Browser setup details leak into normal task prompts.
- The shim changes command semantics.
- Rust owns model-visible CDP again.
- Full eval regresses against raw Codex + browser-harness without a clear
  external explanation.

## First Concrete Milestone

Implement only this first:

```text
Rust starts Python browser-harness worker.
Codex gets browser shim.
browser shim talks directly to worker.
worker returns raw output.
one browser task works.
fixed command trace matches raw browser-harness.
```

No `/profile`, no `/secrets`, no SDK polish until this proves parity.
