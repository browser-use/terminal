# Codex + Browser-Harness vs. new-core — Differences Analysis

**Goal of this doc.** You want to build "Codex (the LLM harness core) + browser-harness (the browse-interaction layer)" as one product. `browsercode` (opencode + a `browser_execute` tool) is the existence proof that this combination works *today*. This doc compares **how new-core currently does it** against that ideal, weighting **browse-interaction internals** and **harness↔browser integration glue** equally, and covering the **harness core only where new-core deliberately diverges from Codex**. The Rust new-core is the committed base; differences are framed as "distance to converge."

> Scope note: this is a **differences doc**, not a plan. The closing section lists *implications* of each gap, not an implementation schedule.

---

## 0. The four codebases at a glance

| Repo | Role here | Stack | What it is |
|---|---|---|---|
| `repos/codex` | The harness-core ideal | Rust | OpenAI Codex CLI: turn loop, context/compaction, tool dispatch, provider streaming. |
| `repos/browser-harness` | The browse-interaction ideal (philosophy) | Python + CDP | Minimal "browser is a coding problem" harness: persistent CDP daemon, agent writes raw Python helpers, agent-authored domain skills. **No agent loop of its own.** |
| `repos/browsercode` | The *working* combination | TypeScript (opencode fork) | opencode harness + a single `browser_execute(code)` tool running JS **in-process** against a **persistent CDP session**. The reference implementation. |
| `new-core/main` | What we're converging | Rust | Codex-faithful Rust harness (`browser-use-agent`) + multi-provider LLM layer (`browser-use-llm`) + a process-spawning CDP control plane (`browser-use-browser`). |

**The one-sentence framing:** new-core's *harness half* is already a close Codex port; the *browse-interaction half* is built on a fundamentally different execution model (process-per-script Python over a TCP bridge) than the ideal (in-process / persistent-session code-as-CDP). The integration glue (tool surface + vision path) differs as a direct consequence.

---

## Part A — LLM harness core: where new-core diverges from Codex

new-core's harness is a deliberate, tested Codex port, so the core *mechanism* matches. The divergences are **sanctioned** (per `DECISIONS.md`) and small relative to the browse layer. They matter here only because browsercode/opencode made *different* choices, and you should know which differences are intentional vs. accidental.

### A.1 Structural parity (same as Codex)
- **Turn loop**: `crates/browser-use-agent/src/turn/loop_driver.rs` (unbounded async loop) → `sampling.rs` (model round-trip + tool fusion) → `dispatch.rs` (tool routing). Mirrors codex `core/src/session/turn.rs:run_turn` + `handlers.rs:submission_loop`.
- **Pure decision core**: `browser-use-agent/src/decision/` holds all heuristics (loop classification, token/drain thresholds) with no I/O — a clean split codex doesn't formalize but is behaviorally identical.
- **Context/token accounting**: `browser-use-agent/src/context/` does **real per-provider** token usage, not estimation; context-message injection mirrors codex's typed `reference_context` (D-1).
- **Event-sourced protocol**: `browser-use-protocol` `EventRecord`/`ModelEvent` ≈ codex `protocol/src/protocol.rs:EventMsg`.

### A.2 Sanctioned divergences from Codex (`DECISIONS.md`)
| ID | Divergence | Why |
|---|---|---|
| D-DIV-1 | **Multi-provider** (OpenAI Responses, Anthropic Messages + Claude-Code OAuth, Ollama, DeepSeek, OpenRouter, Fireworks) vs codex OpenAI-only | Product needs it. Implemented as opencode-style **protocol × provider** split in `browser-use-llm/src/{protocols,route,providers}`. |
| D-DIV-2 | **SQLite is a write-only sink**, never hot-polled; in-memory runtime state; read only on resume | "Reading it hot is slow as hell." |
| D-DIV-3 | **Sync/serial `view_image`** | Screenshots observed in strict order vs. the browser work that produced them. |
| D-DIV-4 | **Browser + Python tool surface** | The product layer. |
| D-DIV-5 | **Codex/ChatGPT backend dropped** entirely | Server-side, can't use. |

### A.3 How this compares to the browsercode/opencode harness (informational)
- opencode's harness (`packages/opencode/src/session/processor.ts` + `llm.ts`) is built on the Vercel `ai` SDK's `streamText()` — **provider-agnostic by delegation**. new-core instead implements each wire format itself (`openai_responses.rs`, `openai_chat.rs`, `anthropic_messages.rs`). new-core's approach is *more* codex-faithful and gives byte-level control; opencode's is thinner but defers to a third-party SDK.
- opencode tools are Effect-Schema `Tool.Def`s with `execute(args, ctx)`; new-core tools are `ToolRuntime` impls behind an orchestrator seam (`tools/orchestrator.rs`, `tools/runtime.rs`) with frozen `Approvable`/`Sandboxable` interfaces. new-core's seam is **richer** (built for future sandbox/guardian) — codex-grade, beyond opencode.

**Takeaway for Part A:** the harness half needs *no convergence work*. It's already at-or-beyond the ideal. The only thing to carry forward is that opencode proves a single `browser_execute`-style tool is enough — you don't need the harness to know anything about browsers (see Part C).

---

## Part B — Browse-interaction internals: the real gap

This is where new-core and the ideal genuinely diverge in architecture, not just style.

### B.1 Execution model — process-per-script vs. persistent in-process

**new-core (`browser-use-browser/src/lib.rs`):** every `start` of a browser script **spawns a fresh OS process**.
- `spawn_browser_script_with_session_registry` (`lib.rs:970`) creates a TCP listener on a random port (`lib.rs:983`), spawns a **bridge thread** (`lib.rs:994`), then spawns a **Python subprocess** (`lib.rs:1021`) running a base64-embedded prelude (`lib.rs:6469`) plus 2 stdout/stderr reader threads (`lib.rs:1031-1032`).
- Python selection walks `LLM_BROWSER_BROWSER_SCRIPT_PYTHON` → `VIRTUAL_ENV`/`.venv` → `uv run` → `python3` (`lib.rs:1430`).
- **Per-script-call cost:** 1 Python interpreter spawn + 1 bridge thread + 1 TCP listener + 2 reader threads. (Codex's own subprocess tools have similar spawn cost — but browsercode eliminated it for the browser specifically.)

**browsercode (`packages/bcode-browser/src/browser-execute.ts`):** agent code runs **in-process** via the JS `AsyncFunction` constructor (`browser-execute.ts:128,166`), invoked as `wrapped(session, snippetConsole)` (`browser-execute.ts:222`). Explicitly: *"No subprocess, no daemon, no Unix socket, no uv"* (`browser-execute.ts:3-6`). Per-call cost ≈ compile a function + run it.

**browser-harness (`src/browser_harness/daemon.py`):** a middle point — a **long-lived daemon** multiplexes one CDP WebSocket; each `browser-harness <<PY` invocation is a thin client that talks to the warm daemon over a Unix socket. Still a subprocess per call, but the *browser connection* is persistent and warm.

**What's actually shared in new-core (the nuance):** the CDP **WebSocket is persistent** even though the Python process isn't. `BrowserSession.connection: Option<Arc<CdpDispatcher>>` (`lib.rs:185`) lives in a global registry (`lib.rs:323`, `BROWSER_SESSIONS` OnceLock at `lib.rs:236`) keyed by session id, reused across calls. The fresh Python process reaches that shared connection by opening a TCP socket back to the Rust **bridge** (`lib.rs:6019`), which does a one-line-JSON request/response per CDP call (`browser_script_helpers.py:26 _bridge`, `:cdp`). So the expensive WebSocket/browser is reused; the **Python runtime and bridge plumbing are torn down and re-erected every script**.

| | new-core | browser-harness | browsercode |
|---|---|---|---|
| Code substrate | Python | Python | JavaScript (TS) |
| Where code runs | fresh subprocess/call | thin subprocess → warm daemon | in-process (`AsyncFunction`) |
| CDP connection | persistent (`Arc<CdpDispatcher>`) | persistent (daemon) | persistent (`Session` in `SessionStore`) |
| Code→CDP path | Python → TCP bridge → Rust dispatcher → WS | Python → Unix socket → daemon → WS | direct JS method call → WS |
| Per-call overhead | Python spawn + bridge + 3 threads | subprocess spawn (daemon warm) | function compile |
| Concurrency | strictly serial | daemon-serialized | per-call isolated buffers |

### B.2 Abstraction level — enum-of-actions vs. raw-code-as-CDP

**new-core** wraps the browser in a typed `BrowserAction` surface (`tools/handlers/browser.rs:263` `BrowserActionKind`: `Command | Start | Observe | Cancel`). The model picks an action and supplies a `script`/`command`/`run_id`. The Python helper library (`browser_script_helpers.py`) then offers a *high-level* API: `goto_url`, `page_info`, `click_at_xy`, `fill_input`, `press_key`, `screenshot`, `wait_for_network_idle`, `http_get`, etc. — i.e., a curated SDK on top of raw CDP.

**browsercode / browser-harness** deliberately give the agent **raw CDP as code** with near-zero abstraction: `await session.Page.navigate(...)`, `await session.Runtime.evaluate(...)`, `session.Input.dispatchMouseEvent(...)`. The `session` object auto-injects `sessionId` on every non-browser-level RPC (`cdp/session.ts:215-217`). The philosophy (browsercode README, browser-harness SKILL.md): *the model already knows CDP and JS; don't put a DSL between them; let it write the helpers it needs.*

This is a genuine philosophical fork:
- new-core = **"give the agent a good browser SDK"** (more guardrails, more curation, Python).
- ideal = **"give the agent the raw protocol and let it code"** (less abstraction, more model leverage, JS).

Note new-core's helpers *do* expose `cdp(method, ...)` raw escape-hatch (`browser_script_helpers.py:26`), so it's not exclusively high-level — but the model-facing default and the docs steer toward the SDK, not raw CDP.

### B.3 Self-improvement / workspace

**browsercode**: the agent writes reusable `.ts` helpers into `<projectDir>/.bcode/agent-workspace/` and imports them in later snippets with cache-bust: `await import("…/scrape_titles.ts?t=" + Date.now())` (`tool/browser-execute.ts:21,47`, `SKILL.md:145-172`). Over a session the agent **builds its own helper library**.

**browser-harness**: stronger version of the same idea — agents write `agent_helpers.py` and, crucially, **`domain-skills/`**: 97+ site-specific playbooks (selectors, timing quirks, working API patterns) that are *agent-generated documentation of what actually works* on amazon/linkedin/github/etc. `goto_url` can surface the relevant domain skill on navigation (`BH_DOMAIN_SKILLS=1`).

**new-core**: no agent-writable browser workspace and **no domain-skills mechanism** in the browser layer. There is a general skills/plugins system in the harness, but nothing that lets the browser agent accrete reusable selectors/helpers/playbooks across runs. This is a **missing capability**, not just a different one — and it's a big part of *why* browsercode/browser-harness "work well": they get better at a site every time they touch it.

### B.4 Connection modes

All three converge on the same connection strategies; new-core has the most modes:
- **new-core** (`lib.rs` `BrowserMode`): `Local | Managed (Rust-owned launch) | RemoteCdp | RemoteCloud`.
- **browsercode** (`cdp/session.ts:81`): env override (`BU_CDP_WS`/`BU_CDP_URL`) → OS profile scan of `DevToolsActivePort` (`:345-404`) → explicit `{wsUrl}`/`{profileDir}`; plus Browser Use cloud via API (`SKILL.md:63`).
- **browser-harness**: local `chrome://inspect`, dedicated debug-port Chrome, or Browser Use cloud.

This axis is **not a gap** — new-core is at parity or ahead.

---

## Part C — Integration glue: how the browser plugs into the harness

This is the half that the user weighted equally, and it's where the execution-model choice (Part B) leaks into the model-facing experience.

### C.1 Tool surface the model sees

**browsercode — one tool, one field that matters:** `browser_execute(code, timeout?, description)` (`tool/browser-execute.ts:23,56-68`). The model writes JS; that's the entire interface. The harness knows *nothing* about browsers — it's a generic tool.

**new-core — a stateful mini-protocol:** `browser` tool with `action ∈ {command,start,observe,cancel}`, plus `script`/`code`, `command`, `run_id`, `session_id`, `timeout_secs`, `observe_timeout_ms` (`tools/handlers/browser.rs:195-277`). The model must manage a **run lifecycle**: `start` returns a `run_id` + `next_observe_ms`, then the model **polls** with `observe` until done, or `cancel`s. This exists because the script runs in a separate process that may outlive a single tool round-trip.

| | browsercode | new-core |
|---|---|---|
| Tool count | 1 (`browser_execute`) | 1 tool, 4 actions |
| Model writes | JS snippet | Python snippet OR command |
| Lifecycle the model manages | none (await inside snippet) | start → observe(poll) → cancel |
| Why | in-process, snippet awaits to completion | separate process; needs explicit observe/cancel |
| Harness coupling | zero (generic tool) | tool encodes a run registry + polling protocol |

**The glue difference in one line:** browsercode pushed *all* browser statefulness into the persistent `session` object so the **tool stays stateless and trivial**; new-core's tool carries a **start/observe/cancel run protocol** because the execution substrate is a detached process. This is the most visible model-facing consequence of the Part B execution-model choice.

### C.2 Vision path — screenshots → model

**browsercode (automatic):** `browser_execute` taps `session.onCallResult` and collects every `Page.captureScreenshot` result's base64 (`browser-execute.ts:204-219`). The Level-2 adapter maps those to `FilePart` attachments with `data:` URLs (`tool/browser-execute.ts:64-81`) which opencode appends to the **next** assistant turn as native vision input. The agent never makes a separate "view image" call — it just calls `captureScreenshot` and *sees* the result next turn.

**new-core (two paths, one of them a separate tool):**
1. **In-script screenshots**: `screenshot()` (`browser_script_helpers.py:633`) calls `Page.captureScreenshot`, writes a JPEG to `ARTIFACT_DIR`, appends `{path,mime_type,label}` to an `__images` list. On run completion these become `images` in `BrowserScriptOutput` (`lib.rs:48`) and the handler re-wraps them via the `__browser_script_content__` stdout marker into vision `ContentPart`s. **This path is automatic-ish**, similar in spirit to browsercode (capture → vision next turn), but routed through files + a stdout marker rather than an in-process tap.
2. **`view_image` tool** (`tools/handlers/view_image.rs`): a **separate, model-initiated, sync, forced-serial** tool (D-DIV-3) that reads a local image file and emits a `view_image:` data-URL. `parallel_safe=false` (`view_image.rs:223`), blocking `std::fs::read`. This is the codex `view_image` tool, intentionally kept serial so images are observed in order.

So new-core *has* an automatic screenshot path, but it's **file-mediated + marker-parsed**, and it also retains a **distinct serial `view_image` tool** as the ordered-observation mechanism. browsercode collapses both into one in-process tap with no separate tool and no file dance.

| | browsercode | new-core |
|---|---|---|
| Capture → vision | `onCallResult` tap, base64 → FilePart, in-process | `screenshot()` → JPEG file → `images[]` → stdout marker → ContentPart |
| Separate view tool | none | `view_image` (sync, serial, D-DIV-3) |
| File written | no (data: URL) | yes (ARTIFACT_DIR) |
| Ordering guarantee | implicit (next turn) | explicit serial `view_image`; plus a 2fps frame-capture thread (`lib.rs:6567`) |

new-core additionally runs a **2fps background frame-capture thread** (`lib.rs:6567`, dedup by Blake2b hash) — a richer recording capability browsercode doesn't have. That's a divergence *in new-core's favor* for observability, at the cost of complexity.

### C.3 Streaming, permissions, parallelism

- **Streaming output**: both stream partial output to the UI. browsercode via `ctx.onChunk` → `ctx.metadata` (`tool/browser-execute.ts:52`). new-core via the `start`/`observe` drain loop returning deltas (`lib.rs:801,908`).
- **Permissions**: browsercode gates on a single `"browser_execute"` permission, default-allow (`tool/browser-execute.ts:39`). new-core routes through the orchestrator/`Approvable` seam (auto-approve today, sandbox=None) — heavier, future-proofed.
- **Parallelism**: both serialize the browser. browsercode isolates per-call console/screenshot buffers but shares one `Session`; new-core sets `parallel_safe=false` (`browser.rs:2064`) and runs on `spawn_blocking` (`browser.rs:2161`) with registry checkout/return mutual exclusion (`lib.rs:400-426`).

No major gap here — new-core is heavier but functionally equivalent or ahead (the orchestrator seam is real Codex-grade infrastructure opencode lacks).

---

## Part D — Side-by-side summary

### D.1 The differences that matter, ranked

1. **Execution substrate (B.1)** — *the headline.* new-core spawns a Python process (+ bridge + threads) per script `start`; the ideal runs code in-process / against a warm daemon. The CDP connection is already persistent in new-core, so the gap is specifically the **per-call code-runtime spawn and the TCP-bridge indirection**, not the browser connection.
2. **Tool surface / lifecycle glue (C.1)** — new-core's `start/observe/cancel` polling protocol is a *direct consequence* of #1. The ideal's tool is a stateless `browser_execute(code)`. Fixing #1 is what lets you collapse the tool.
3. **Self-improvement / domain-skills (B.3)** — *a missing capability.* browsercode's workspace and browser-harness's agent-authored domain-skills are a major reason they "work well." new-core's browser layer has neither.
4. **Code substrate & abstraction level (B.2)** — Python-with-an-SDK vs. JS-as-raw-CDP. Philosophical; affects how much the model leverages its own CDP/JS knowledge.
5. **Vision path (C.2)** — both automatic, but new-core is file+marker-mediated with a separate serial `view_image`; ideal is a single in-process tap. Lower-priority; new-core's version works and adds frame-capture.

### D.2 Where new-core is already at-or-ahead-of the ideal
- Harness core (Part A): codex-faithful, real token accounting, richer orchestrator/sandbox seam.
- Multi-provider (D-DIV-1): more providers than codex, cleaner than relying on the `ai` SDK.
- Connection modes (B.4): Local/Managed/RemoteCdp/RemoteCloud.
- Observability: 2fps frame capture, structured `browser_events`, diagnosis (`BrowserIssueDiagnosis`).
- Event-sourced SQLite durability (D-DIV-2) and ordered `view_image` (D-DIV-3) are deliberate, defensible divergences.

### D.3 The crux

> The ideal's "it just works" comes from **collapsing the browser into a persistent in-process session that the agent drives with raw code, plus an accreting skill/workspace memory.** new-core has the **persistent connection** but not the **persistent code-runtime** — it re-spawns Python per script and bridges over TCP — which forces a heavier `start/observe/cancel` tool protocol and leaves out the workspace/domain-skills memory. The harness half is done; the browse half is one architectural decision (where and how agent code executes) away from the ideal, plus one missing capability (skill memory).

---

## Part E — Implications for convergence (not a plan)

Framed as "Rust new-core is the base, port the ideal into it." Each is an *implication of a gap above*, not a committed task.

- **Decide the code substrate (resolves #1, #2, #4).** The deepest question: do you want the agent writing **JS-as-CDP** (the ideal) or keep **Python-with-an-SDK**? In Rust this is the fork:
  - *Keep Python, kill the spawn*: replace per-script `python3` spawn with a **persistent Python worker** (you already have `browser-use-python-worker` — a long-lived subprocess wrapper). A warm worker holding the CDP bridge collapses most of #1's overhead and could let the tool drop `observe`/`cancel` in the common case. This is the smaller move and stays in the product's current language.
  - *Adopt JS-as-CDP*: embed a JS engine (`deno_core`/V8) so the agent's snippet runs in-process against a Rust-side `Session`, à la browsercode. Bigger lift, closest to the ideal, unlocks the raw-CDP philosophy and matches `repos/cdp-use`/`browser-harness-js`.
- **Collapse the tool surface (resolves #2)** *iff* the substrate change removes detached-process lifetimes — then `browser` can become `browser_execute(code)` with no `observe`/`cancel`, matching browsercode's stateless glue.
- **Add a browser workspace + domain-skills memory (resolves #3).** An agent-writable `.../agent-workspace/` for reusable helpers and an agent-authored `domain-skills/` surfaced on navigation. This is additive and independent of the substrate decision — and arguably the highest *quality* leverage per effort, since it's what makes the ideal compound over time.
- **Optionally unify the vision path (resolves #5).** Once code runs in-process, a `captureScreenshot` tap → vision-attachment can replace the file+marker route, while you decide whether to keep the serial `view_image` (D-DIV-3) as the explicit ordering tool.

The first bullet is the decision everything else hangs on. The rest are consequences or independent additions.
</content>
</invoke>
