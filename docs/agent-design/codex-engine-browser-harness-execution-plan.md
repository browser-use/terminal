# Codex Engine + Browser-Harness Execution Plan

Status: active planning doc.

This supersedes the older Rust-owned-CDP direction where it conflicts with this
document. The target is not a cleaner browser abstraction. The target is the
same model-visible behavior as raw Codex + browser-harness, while keeping the
Browser Use Terminal product shell: TUI, Python SDK, settings, store, events,
history, and browser setup UX.

## Brutal Conclusions

1. Rust-owned CDP is the wrong first target for benchmark parity.
   It is attractive architecturally, but experiments showed it can degrade
   performance. Browser-harness/Python should own model-visible browser/CDP
   interaction.

2. The TUI and SDK do not need a rewrite.
   The seam already exists. Replace the old agent executor with a CodexEngine
   adapter that emits the same Browser Use store/runtime events.

3. Codex must be embedded through app-server crates, not `codex exec`.
   The right seam is `codex-app-server-client` /
   `InProcessAppServerClient`, behind a Browser Use adapter. The TUI/SDK should
   never speak Codex protocol directly.

4. The first production-quality implementation should use one Codex app-server
   instance per Browser Use session.
   Shared app-server concurrency is possible later, but app-server-scoped
   config, auth, plugins, skills, MCP, and state make shared mode too risky
   before thread-scoped env/skills/raw-event patches exist.

5. The browser-harness manager branch is the right browser direction, but not a
   finished product boundary.
   Use `browser-use/browser-harness@codex/browser-manager-impl` as the base,
   then harden identity, locks, browser activation, cloud-profile args, and
   public/private fields before relying on it for product features.

6. Other LLMs require a real local Responses-compatible gateway.
   Codex provider config is Responses-shaped. Gemini, Anthropic, OpenRouter
   chat, and random providers do not become compatible just because they expose
   an OpenAI-looking endpoint.

7. `96/106` is a high-water target, not a magic reproducible constant.
   We should prove parity against a fresh contemporaneous raw Codex +
   browser-harness run using the same judge. Historical `96/106` remains the
   capability target.

## North Star

The model sees the browser like raw Codex + browser-harness.

Rust owns:

- terminal UI
- Python SDK server
- settings and credentials
- session/run IDs
- SQLite store and event schema
- runtime socket and cancellation
- artifact roots
- process supervision
- eval evidence

Python browser-harness owns:

- browser lifecycle
- local/cloud/managed browser selection
- local profile detection and attach
- cloud browser creation and cleanup
- CDP websocket ownership
- page observation and browser actions
- model-visible browser command output

Codex owns:

- model loop
- shell/exec tool behavior
- tool-call protocol
- provider request/stream semantics
- context and turn behavior
- final assistant message generation

## Non-Negotiable Invariants

1. Model-visible browser output is never summarized, rewritten, normalized,
   truncated, or reinterpreted by Rust.

2. The benchmark path exposes the raw browser-harness skill and command surface.
   No Browser Use structured browser tool, hidden audit tool, local search tool,
   or product setup dump is added to the model loop.

3. TUI and SDK consume Browser Use events, not Codex app-server events.
   Codex protocol is an internal implementation detail.

4. Browser settings are product intent.
   `/browser`, `/profile`, `/sync-cookies`, domains, secrets, and email are
   stored and applied out of band. Do not depend on the model to configure the
   browser correctly.

5. Codex auth/config home and browser-harness runtime state are separate.
   Do not use a per-session `CODEX_HOME` trick that breaks Codex login, leaks
   auth state, or makes concurrency nondeterministic.

6. Every audited run stores full final answers, raw Codex events, Browser Use
   projected events, browser-harness worker/manager events, artifacts, and judge
   packets.

7. For Internal_Bench_hard parity, never use an 80-turn cap.

## Target Architecture

```text
TUI / CLI / Python SDK
  -> Browser Use store/runtime/events
  -> CodexEngine adapter
       -> per-session Codex InProcessAppServerClient
       -> browser-harness supervisor env
       -> raw event sink
       -> Browser Use event mapper
       -> final-answer/artifact capture
       -> optional local Responses gateway for non-OpenAI models
  -> Codex thread/turn
       -> shell/exec
       -> browser / browser-harness command shim
       -> Python browser-harness manager/worker
       -> browser/CDP/cloud/local profile
```

The hot path must remain:

```text
Codex -> shell/exec -> browser-harness command -> Python harness -> browser/CDP
      <- exact stdout/stderr/exit code <-
```

Rust may supervise this path. Rust must not become part of browser reasoning.

## Component 1: CodexEngine Adapter

Create a new Rust crate, for example `crates/browser-use-codex-engine`.

Responsibilities:

- map Browser Use `session_id` to Codex `thread_id`
- map Browser Use runs/followups to Codex turns
- start and stop one `InProcessAppServerClient` per session for v0
- start `ThreadStart` with cwd, model, provider, instructions, workspace roots,
  config, browser-harness env, and raw-events enabled
- start `TurnStart` for initial task and next-turn followups
- use `TurnSteer` for active-turn user input where safe
- use `TurnInterrupt` for cancel
- drain `next_event()` continuously
- persist raw Codex events to JSONL
- project Codex events into Browser Use store/runtime events
- capture final answer from message deltas/items, not `TurnCompleted` alone
- on completion, write `session.done` with complete result or `session.failed`
  with an actionable error

Do not expose Codex thread/turn IDs to the Python SDK or TUI except as debug
metadata.

### Required Codex Fork Patches

These are the patches likely required in `new-terminal-exp-codex-fork`.

1. Lossless raw event delivery.
   Raw events cannot be optional or lossy for evals. `Lagged` must be treated as
   fatal in audited runs.

2. Thread-scoped shell env.
   Browser-harness env must be attached to the thread/tool execution context,
   not global process env.

3. Thread-scoped skills or inline skills.
   The short-term path can inject browser-harness skill text through
   instructions. The durable path needs per-thread skill roots or inline skill
   specs so concurrent sessions do not leak skill config.

4. Auth-refresh support if relying on Codex login for long-running sessions.
   The in-process client currently has weaker managed-auth behavior than the
   full CLI path.

5. Provider-per-turn only if the product truly needs changing provider/model
   inside the same session. Otherwise document provider fixed per session.

## Component 2: Browser-Harness Manager/Supervisor

Use the browser-harness branch:

`https://github.com/browser-use/browser-harness/tree/codex/browser-manager-impl`

Browser Use Terminal should not reimplement browser/CDP ownership in Rust.
Instead it passes product intent into browser-harness manager state.

Browser Use Terminal owns:

- selected backend: local Chrome, local Chromium, managed, cloud
- selected local profile ID
- cloud profile/cookie sync intent
- domain allow/block policy
- secrets and disposable email config
- API key storage
- state/artifact directory
- eval evidence

Browser-harness manager owns:

- local profile discovery/proof
- local attach and marker-tab behavior
- cloud browser creation
- CDP endpoint and target ownership
- browser IDs
- cleanup

### Manager Hardening Before Product Use

Fix these in browser-harness before making it the product default:

- `browser_new()` should activate the created browser if the skill/docs imply
  subsequent helpers use it.
- Manager `switch`, `close`, and `close_owned` need real identity/access checks.
- `BH_RUN_ID`, `BH_AGENT_ID`, and `BH_PARENT_AGENT_ID` must be explicit from
  Browser Use Terminal, not guessed from cwd.
- `BH_MANAGER_ROOT` and `BH_MANAGER_SOCKET` must be explicit under the Browser
  Use state/artifact root.
- Do not expose provider browser IDs, CDP URLs, or secrets in public manager
  responses. Public `live_url` is okay if intended.
- Add cloud profile/profile-name arguments to manager cloud creation.
- Add root-level single-manager locking and cleanup/sweeper behavior.

### Model-Visible Surface

For benchmark parity, use the raw browser-harness skill text from the pinned
branch. Do not add Browser Use product commands to the model prompt.

For product UX, apply `/browser`, `/profile`, `/sync-cookies`, domains,
secrets, and email through manager env/config/admin APIs before the Codex turn.
The model may use normal browser-harness helpers, but should not be responsible
for choosing the user's profile or recovering product setup state.

## Component 3: Keep The Existing TUI/SDK Contract

Keep:

- `browser-use-store`
- `browser-use-runtime`
- `browser-use-tui` rendering and input model
- CLI JSON-RPC `sdk-server`
- Python SDK `RuntimeClient`
- history and transcript projection
- cancellation and mailbox concepts

Replace:

- old `browser-use-agent` executor/driver path
- old provider loop for agent runs
- old browser/CDP runtime when manager mode is active

The main replacement seams are:

- `crates/browser-use-tui/src/runtime.rs`
  - `spawn_tui_agent_run`
  - `prepare_tui_agent_run`
  - followup submission
  - cancel submission

- `crates/browser-use-cli/src/main.rs`
  - SDK `agent.run`
  - SDK child runner equivalents

- old browser runtime paths
  - ensure they are bypassed in Codex+browser-harness mode
  - leave legacy/debug mode only if useful

### Event Mapping Contract

CodexEngine must emit the event shapes the current UI/SDK already understand:

- `agent.run.started`
- `model.turn.request`
- `model.thinking_delta`
- `model.stream_delta`
- `model.turn.response`
- `model.turn.error`
- `model.usage`
- `token_count`
- `tool.started`
- `tool.output_delta`
- `tool.output`
- `tool.failed`
- `exec_command.begin`
- `exec_command.output_delta`
- `exec_command.end`
- `browser.connected`
- `browser.live_url`
- `browser.state`
- `artifact.created`
- `session.done`
- `session.failed`
- `session.cancel_requested`
- `session.cancelled`

Raw Codex events should also be persisted as `codex.raw_event` or sidecar JSONL
for audit, but the TUI should not depend on those raw event names.

## Component 4: Non-OpenAI Providers

Do not fork the agent loop per provider. Codex remains the runtime.

For OpenAI/Codex:

- use Codex native provider config and auth
- this is the benchmark path

For Anthropic/Gemini/OpenRouter/custom:

- run a local Browser Use Responses gateway
- configure Codex model provider to point at `http://127.0.0.1:<port>/v1`
- keep provider keys in Browser Use settings/env, not Codex config
- translate Codex Responses requests to browser-use-llm provider requests
- translate provider streams back into OpenAI Responses SSE events

Initial gateway scope:

- text input/output
- function tools
- tool results
- HTTP SSE
- basic usage
- provider errors
- cancellation/backpressure

Do not claim first-class support yet for:

- reasoning summaries/encrypted reasoning
- OpenAI hosted tools
- WebSocket Responses
- complex multimodal payloads
- provider-native response state

This gateway is product compatibility work, not part of proving the OpenAI
Internal_Bench_hard score.

## Implementation Phases

### Phase 0: Freeze References And Contracts

Output:

- pin Browser Use repo commit
- pin Codex fork commit
- pin browser-harness branch commit
- record dataset SHA for `/home/exedev/datasets/Internal_Bench_hard.json`
- write event mapping fixtures
- write browser-harness command preservation tests

Tests:

```bash
cargo fmt --check
cargo test
uv run --with pytest python -m pytest -q
```

Eval:

- dry-run Internal_Bench_hard runner
- verify no 80-turn cap can launch
- run a fresh raw Codex + browser-harness baseline if credentials/quota allow

### Phase 1: Single-Session CodexEngine Proof

Implement one session with:

- one `InProcessAppServerClient`
- one Codex thread
- one turn
- raw browser-harness skill/instructions
- browser-harness manager env
- raw event JSONL
- Browser Use event projection
- full final-answer capture

Exit criteria:

- TUI can run a trivial non-browser task
- TUI can run one browser task in cloud mode
- SDK can run one task and stream events
- final answer appears in store/history/artifacts
- browser-harness command output is byte-preserved

### Phase 2: Followups, Cancel, Resume Shape

Implement:

- active followup to `TurnSteer` where safe
- queued followup as next `TurnStart`
- cancel to `TurnInterrupt`
- durable Browser Use event recording for all three
- dead-runtime fallback cancellation

Exit criteria:

- active TUI followup works
- queued TUI followup works
- SDK followup works
- cancel stops Codex, worker, and browser-harness manager cleanly
- history remains readable

### Phase 3: Browser-Harness Manager Product Wiring

Implement/harden:

- explicit `BH_MANAGER_ROOT`
- explicit `BH_MANAGER_SOCKET`
- explicit identity env
- backend/profile/cloud-profile intent from TUI/SDK settings
- cloud key/auth path injection without leaking secrets
- manager lifecycle evidence events
- public `live_url` display
- no Rust CDP ownership in manager mode

Exit criteria:

- `/browser` selects backend for future tasks
- `/profile` selects local profile for future local tasks
- `/sync-cookies` feeds cloud profile intent
- SDK `browser.set_backend` and `browser.set_profile` feed the same intent
- cloud/local switching clears stale state
- manager cleanup runs on done/fail/cancel

### Phase 4: Remove Or Quarantine Old Agent Core

After CodexEngine handles the product path:

- make CodexEngine the default agent runtime
- keep old core only behind a debug feature or delete it
- remove duplicate browser/CDP ownership from default mode
- remove product browser tools from the model-visible benchmark surface

Exit criteria:

- `task tui:dev` uses CodexEngine by default
- Python SDK uses CodexEngine by default
- old browser runtime cannot accidentally handle benchmark browser calls

### Phase 5: Eval Proof

Run in increasing confidence:

1. 5 task smoke on Internal_Bench_hard.
2. 20 task hard subset.
3. Full 106 task Internal_Bench_hard, OpenAI/Codex, cloud browser, no 80 cap.
4. Same locked judge as the raw baseline.
5. Task-by-task comparison against fresh raw Codex + browser-harness.

Preferred full command:

```bash
RUN_ID="ibh-codex-engine-$(git rev-parse --short HEAD)-$(date -u +%Y%m%d-%H%M%S)"
ROOT="/home/exedev/eval-runs/$RUN_ID"

scripts/run-internal-bench-hard-openai.sh \
  --run-id "$RUN_ID" \
  --root "$ROOT" \
  --judge
```

Green criteria:

- 106 unique tasks
- 106 judge packets
- 106 judgments
- no missing event logs
- no malformed judge chunks
- no output/artifact truncation that affects evidence
- strict judged score >= 93/106, or within 3 tasks of fresh raw baseline
- zero current-only regressions traced to Rust rewriting/hiding browser evidence

### Phase 6: Provider Gateway

Only after OpenAI/Codex parity is stable:

- implement local Responses gateway
- start with Anthropic or OpenRouter chat
- then Gemini
- add conservative model metadata
- add streaming/tool-call fixtures
- wire `/model` and SDK provider selection to gateway-backed Codex provider IDs

Exit criteria:

- Gemini/Anthropic can drive CodexEngine through the same Browser Use event
  contract
- gateway emits valid Responses SSE ending in `response.completed`
- tool calls round-trip correctly
- provider disconnect cancels upstream work

## What This Should Improve In Evals

Current-only failures versus raw Codex + browser-harness should fall into these
targeted buckets:

- Browser observation/action divergence.
  Fixed by keeping Python browser-harness as the model-visible browser owner.

- Final/result capture mismatch.
  Fixed by capturing final text from Codex message items/deltas and storing full
  result artifacts, not stdout tails or `TurnCompleted` alone.

- Missing evidence in judge packets.
  Fixed by raw Codex JSONL, Browser Use event projection, browser-harness worker
  events, and artifact roots per task.

- Turn exhaustion on long tasks.
  Fixed by preserving `--max-turns 10000` and rejecting accidental 80-turn runs.

- Browser setup/profile/cloud drift.
  Fixed by explicit manager env, explicit identity, and TUI/SDK settings applied
  out of band before the turn.

- Tool surface confusion.
  Fixed by exposing only the raw browser-harness/Codex-style model surface in
  benchmark mode.

The known previous simple-harness failures to re-check first are:

```text
6dpbhs 82kkzm 8hyexf afeyuh jgzlma l3gywi mly4ly mvxpj4 q85jsg swebnv y72ivg
```

Those were current-only misses relative to the historical raw failure set. They
should be analyzed task by task after the first full CodexEngine run. Do not
claim a code fix solved them unless the artifacts and events show the failure
class changed in the expected direction.

## Red-Team Challenges

### Is this exactly raw Codex + browser-harness?

No. It is exactly raw Codex + browser-harness only at the model-visible browser
interaction layer. The surrounding runtime is Browser Use Terminal. That is why
the acceptance criterion is behavioral parity under the same judge, not a claim
that every internal byte is identical.

### Could per-session Codex app-server be too slow?

Yes. But correctness wins first. A shared app-server is only safe after
thread-scoped env, thread-scoped skills, lossless raw events, and backpressure
behavior are proven. Otherwise concurrency bugs become invisible score noise.

### Could the browser-harness manager branch itself hurt performance?

Yes, if it changes what the model sees or inserts confusing setup behavior.
For benchmark parity, pin the raw skill and keep product setup out of the model
prompt. The manager should change ownership and lifecycle, not browser
semantics.

### Could hiding profile helpers from the model reduce capability?

For benchmark/cloud parity, no. For local product UX, profile choice should come
from `/profile`, not model guesswork. If raw browser-harness skill exposes
profile helpers, keep them available, but do not depend on the model to use
them for product setup.

### Could the provider gateway harm the 93+ target?

It should not be in the benchmark path. Prove OpenAI/Codex first. Gateway work
is for product provider breadth after the core path is stable.

### Could event mapping make the TUI look good while the agent is bad?

Yes. That is why eval judgment must use artifacts, raw Codex events, native
Browser Use events, and task files. Runner success and pretty UI events are not
correctness.

### Could old browser code accidentally still run?

Yes. This is one of the biggest risks. In CodexEngine manager mode, old
`browser-use-browser` and old browser tool handlers must be bypassed or
quarantined. The benchmark surface should fail fast if legacy browser tools are
registered.

## Definition Of Done

The implementation is not done when `task tui:dev` starts.

It is done when:

- `task tui:dev` uses CodexEngine by default
- Python SDK `Agent.run` uses CodexEngine by default
- TUI followup, queued followup, cancel, history, model selection, browser
  settings, profile settings, and SDK run all work
- browser-harness/Python owns model-visible CDP
- old browser runtime is not in the benchmark path
- OpenAI/Codex Internal_Bench_hard full run is judged
- fresh raw Codex + browser-harness is judged or explicitly recorded as missing
- task-by-task delta is explained
- score is >= 93/106 or within 3 tasks of fresh raw baseline
- any remaining miss has a code/evidence/root-cause classification

## High-Level Picture

```text
                         Browser Use Terminal
                  TUI / CLI / Python SDK / Settings
                                  |
                                  v
                 +-----------------------------------+
                 | Rust product runtime              |
                 |                                   |
                 | - sessions and history            |
                 | - SQLite events                   |
                 | - SDK JSON-RPC                    |
                 | - credentials and settings        |
                 | - eval artifacts and evidence     |
                 | - process supervision             |
                 +-----------------+-----------------+
                                   |
                                   v
                 +-----------------------------------+
                 | CodexEngine adapter               |
                 |                                   |
                 | Browser Use session/run           |
                 |      -> Codex thread/turn         |
                 | Codex raw events                  |
                 |      -> Browser Use events        |
                 | final assistant message           |
                 |      -> session.done/artifacts    |
                 +-----------------+-----------------+
                                   |
                                   v
                 +-----------------------------------+
                 | Codex app-server / core           |
                 |                                   |
                 | - model loop                      |
                 | - shell/exec tools                |
                 | - provider streaming              |
                 | - tool-call semantics             |
                 +-----------------+-----------------+
                                   |
                                   v
       model sees this only:  browser / browser-harness command surface
                                   |
                                   v
                 +-----------------------------------+
                 | Python browser-harness            |
                 |                                   |
                 | - browser manager                 |
                 | - local/cloud/profile ownership   |
                 | - CDP websocket ownership         |
                 | - page observation/actions        |
                 +-----------------+-----------------+
                                   |
                                   v
                    Local Chrome / Chromium / Cloud Browser
```

The simple version:

Browser Use Terminal remains the product. Codex becomes the agent brain.
Browser-harness remains the browser hands and eyes.

Rust should coordinate the system, persist the truth, and make the user
experience work. Rust should not reinterpret the browser for the model.

The model-facing browser layer should look boringly identical to raw Codex +
browser-harness. All product richness, including profiles, cloud selection,
secrets, domains, email, SDK calls, history, and UI state, should sit around
that layer instead of inside the model's browser reasoning path.

The highest-confidence path is:

```text
first prove CodexEngine + raw browser-harness works
then wire TUI/SDK/product settings around it
then delete or quarantine the old core
then run the full judged benchmark
then add other providers through the Responses gateway
```

The thing to avoid is building a second browser abstraction and hoping the model
learns it. The benchmark already tells us what works: Codex with the
browser-harness affordance. The product work is to embed that affordance cleanly
without changing what made it strong.
