# Codex + Browser-Harness Core Plan

Status note, 2026-06-18: this was an earlier Rust-CDP ownership direction.
The active architecture is
`docs/agent-design/browser-harness-supervisor-implementation-plan.md`: Rust
supervises product lifecycle, TUI/SDK/session/store/process state, while Python
browser-harness owns model-visible CDP/browser interaction. Keep this document as
historical planning context only where it conflicts with that supervisor plan.

## Goal

Build Browser Use Terminal around a Rust-owned browser harness core while keeping
the model-facing behavior as close as possible to the proven Codex +
browser-harness arm. The target is a strict judged score of 93%+ on `real_v8`,
measured task-by-task against the Codex + browser-harness reference run.

The important constraint is not "everything must be Rust". The important
constraint is behavioral parity:

- same simple browser skill contract
- same shell/exec affordance for browser snippets
- same retrieved-data grounding standard
- same complete final-answer capture
- same per-task workspace/home isolation
- stronger lifecycle ownership than the Python subprocess daemon path

Rust ownership of CDP and browser management is still a hard product
requirement. Python compatibility may exist at the helper/snippet layer, but
Rust should own browser creation/attachment, CDP websocket lifetime, target/tab
state, cleanup, event recording, and SDK/TUI integration.

## Interfaces To Preserve

### Terminal UI

The TUI should keep talking to the normal Rust store/session runtime. It should
not know whether the browser is backed by Python browser-harness, a Rust harness
daemon, managed Chrome, cloud Chrome, or local Chrome.

Required TUI-facing contract:

- task/session lifecycle remains in SQLite events
- browser status, screenshots, current URL, cleanup, and failures are emitted as
  normal session events
- user-visible history and completed output read from the same `final.txt`,
  result files, and event projections as CLI runs
- no model-facing benchmark code in the TUI

### Python SDK

The Python package should use the existing Rust `sdk-server` direction, not a
new ad hoc subprocess protocol. Python is a client; Rust owns the runtime.

Required SDK-facing contract:

- `browser-use-terminal` Python entrypoints continue to work
- Python can create a task, stream events, send follow-up input, stop a task, and
  read final/result artifacts
- Python can request the Codex-like simple harness profile
- Python does not own CDP, daemon cleanup, or browser process lifecycle

## Target Architecture

```text
             TUI
              |
              v
      BrowserUseRuntime  <----- CLI
              ^
              |
        sdk-server
              ^
              |
        Python SDK

BrowserUseRuntime
  -> Provider runner: Codex / Anthropic / Gemini / OpenRouter / other
  -> Tool surface: Codex-like shell/exec + minimal safe built-ins
  -> Harness core: Rust-owned browser session lifecycle
  -> Store: SQLite events + artifacts + final capture
```

The provider layer is interchangeable. The browser harness layer is not
provider-specific.

## Codex Crate Strategy

Build the agent-runtime spine on Codex crates where doing so keeps behavior
closer to the reference:

- provider request/stream semantics
- tool schemas and tool-call dispatch behavior
- shell/unified-exec behavior
- context assembly, config, model catalog, and auth behavior
- rollout/history/finalization semantics
- MCP/web-search/view-image/apply-patch style local tools

Do not make the product runtime a wrapper around `codex exec`. The TUI, CLI, and
Python SDK need a library/runtime API, not a CLI subprocess as the core.

Do not put browser ownership inside Codex crates. Browser/CDP management is a
Browser Use Terminal product responsibility:

- browser create/attach/recover/stop
- local, managed, and cloud browser modes
- CDP websocket routing
- target/tab lifecycle
- browser event buffering
- screenshots/downloads/artifacts
- cleanup and leak prevention
- SQLite event integration for TUI/SDK

Recommended dependency shape:

```text
browser-use-agent
  -> codex runtime crates or pinned codex-derived crates
  -> browser-use-harness
  -> browser-use-store / protocol

browser-use-harness
  -> CDP/browser management
  -> browser-harness-compatible helper contract
  -> no dependency on Codex provider internals

browser-use-tui / sdk-server / CLI
  -> browser-use-agent runtime API
```

If upstream Codex crates are too coupled to the Codex workspace, vendor or fork
only the needed crates at a pinned commit and keep a small compatibility layer.
The rule is still behavioral: direct crates are preferred when they reduce drift;
copying semantics is acceptable only when direct dependency would make the
product brittle.

## Harness Core Shape

Introduce a real Rust crate, likely `browser-use-harness`, with a narrow public
API:

```rust
pub struct HarnessSession;

pub struct HarnessSessionSpec {
    pub session_id: String,
    pub state_dir: PathBuf,
    pub cwd: PathBuf,
    pub artifact_root: PathBuf,
    pub browser_mode: BrowserMode,
    pub simple_contract: bool,
}

impl HarnessSession {
    pub async fn prepare(spec: HarnessSessionSpec) -> Result<Self>;
    pub fn model_env(&self) -> HashMap<String, String>;
    pub fn skill_markdown(&self) -> &str;
    pub async fn cleanup(self) -> Result<HarnessCleanupReport>;
}
```

Internally it owns:

- per-session home/workspace/domain-skill layout
- runtime/tmp dirs
- browser skill materialization
- local/cloud/managed browser connection metadata
- CDP websocket lifecycle
- CDP request routing and event subscription/buffering
- target/tab ownership
- event buffering
- close-tab and cleanup guarantees
- artifact paths for screenshots/tool output/final mirroring

The current `simple_harness.rs` becomes the compatibility adapter first, then
shrinks into calls to `browser-use-harness`.

## Do Not Change First

Do not start by replacing the model-facing browser-harness API with a new
structured browser tool. That is likely cleaner, but it changes the agent's
learned affordances and risks losing the Codex-like behavior.

The first target should be lifecycle and transport parity:

- keep `browser-harness <<'PY' ... PY` compatibility
- keep the packaged browser skill
- keep shell/exec as the main browser interaction path
- replace unmanaged per-session daemon behavior with Rust-owned lifecycle

## Phased Implementation

### Phase 0: Lock The Reference

Run and preserve a Codex + browser-harness reference under the same dataset,
model, cloud browser mode, and judging rubric.

Artifacts:

- `reference/judge_packets.json`
- strict task verdicts
- per-task event/transcript evidence
- list of reference failures, if any

Exit criteria:

- reference score is known
- reference artifacts are complete enough for value tracing
- our comparisons can say "task X differs because ..." rather than guessing

### Phase 1: Freeze The Simple Contract In Our Runtime

Make `simple_harness=true` the explicit benchmark/product profile:

- Codex-like tool surface
- packaged browser skill
- per-session `CODEX_HOME`
- per-session workspace and tmp dirs
- full final capture from `session.done`
- SQLite event mirror to `events.jsonl`
- no 80-turn cap for benchmark runs unless explicitly requested
- wall-clock/task timeout remains as a safety valve

Exit criteria:

- real_v8 runner completes on cloud browsers without runtime crash
- no provider 429s or auth failures
- no missing final artifacts
- cleanup event for every completed session

### Phase 2: Strict Side-By-Side Judge Loop

For every run, build `judge_packets.json` from SQLite + artifacts, not stdout.

For every failed/partial/suspicious task:

```text
task -> our artifact -> reference artifact -> first causal difference
     -> failure class -> code/runtime/prompt area -> proposed fix
```

Failure classes should include:

- incomplete final
- weak grounding
- wrong source
- listing/location mismatch
- model-authored Python/JS bug
- browser-harness helper gap
- CDP/session/tab lifecycle bug
- turn/time budget issue
- site access issue

Exit criteria:

- score report has strict score, runner score, and clean score
- every miss is tied to a concrete task
- every proposed fix names the tasks it should improve

### Phase 3: Rust Harness Lifecycle Core

Move lifecycle ownership from detached Python daemon behavior into Rust:

- Rust-owned per-session harness registry
- Rust-owned CDP websocket connection
- Rust-owned browser create/attach/recover/stop for local, managed, and cloud
- Rust-owned target/tab cleanup
- Rust-owned runtime dir cleanup
- bounded process/thread count
- health checks and restart semantics
- event emission into the normal store

Keep Python helper compatibility initially. The model can still invoke
`browser-harness <<'PY' ... PY`, but the backing session is Rust-supervised.

Exit criteria:

- 25-way benchmark run is a required acceptance gate, not an optional stress
  curiosity
- 25-way run does not leak tabs, daemons, sockets, or threads
- cleanup count matches session count
- no local `WouldBlock` thread-spawn failures
- task outputs match Phase 1 or improve

### Phase 4: Helper Parity Gaps

Only after strict side-by-side judging identifies gaps, port or patch helper
behavior:

- network event buffering
- `wait_for_network_idle`
- tab switching/session attach
- downloads
- screenshot capture
- DOM/text extraction helpers
- robust click/type/select helpers

Each helper change must name the failed tasks it targets.

Exit criteria:

- targeted tasks improve in hard-case reruns
- no regression in tasks that already matched the reference

### Phase 5: Provider-Neutral Productization

Expose the same runtime through:

- TUI
- CLI
- Python SDK via `sdk-server`
- future app/server surfaces

Provider-specific code stays in provider adapters. Harness/session behavior stays
identical across Codex, Anthropic, Gemini, OpenRouter, and local-compatible
providers.

Exit criteria:

- same task can run under at least Codex + one non-Codex provider
- TUI and Python SDK see the same event stream and final artifacts
- provider changes do not alter browser harness semantics

## Eval Loop

Use this loop after every phase:

1. Run a small hard-case set.
2. Judge strictly from artifacts/events.
3. Compare each miss to Codex + browser-harness.
4. Patch only the highest-confidence generalizable gap.
5. Re-run the affected tasks.
6. Run full `real_v8` only after the hard-case set is stable.

For full runs:

- cloud browsers are the benchmark default; local browser runs are diagnostics
  unless explicitly labeled otherwise
- remove the 80-turn cap or set it high enough that it is not the limiting
  variable
- keep a wall-clock timeout
- use concurrency 4 only for the first debug artifact after risky lifecycle
  changes
- use concurrency 25 for the real acceptance run, matching the intended product
  evaluation workload
- treat 25-way failures as lifecycle/runtime bugs unless the artifacts show a
  real provider/site failure
- report runner score separately from judged score

## Benchmark Goal

The useful `set_goal` should be outcome-based and evidence-based:

```text
Reach 93%+ strict judged score on real_v8 with Browser Use Terminal's Rust
runtime, using the Codex-like simple harness contract, while preserving TUI and
Python SDK integration. Every miss must be compared task-by-task against the
Codex + browser-harness reference and mapped to a concrete runtime, helper,
prompt, provider, or site-access cause before changing implementation.
```

This goal is intentionally not "rewrite browser-harness in Rust". Rust is a
means to own lifecycle and product integration. The score target comes from
preserving the proven browser-harness behavior, then removing the specific gaps
shown by judged task failures.
