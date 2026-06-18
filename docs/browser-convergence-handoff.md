# Browser convergence — handoff brief (lessons learned)

**Audience:** an agent picking up the "make new-core's browser layer like browsercode" work with no prior context.
**Status:** investigation complete; no code changed yet.
**Companion docs (read these too):**
- [`codex-browser-harness-differences.md`](./codex-browser-harness-differences.md) — *what* differs, in full.
- [`browsercode-convergence-plan.md`](./browsercode-convergence-plan.md) — *the phased steps*.

This brief is the standalone summary: the goal, the verified facts (with `file:line`), the feasibility findings, the recommended path, the gotchas, and the open decisions. Everything below was verified by reading the code; treat citations as load-bearing but re-confirm before editing (line numbers drift).

---

## 1. The goal, in one paragraph

new-core is "Codex (LLM harness) + browser-harness (browse interaction)" as one Rust product. The **harness half is already a faithful, tested Codex port — leave it alone.** The **browse-interaction half** is built on a different execution model than the reference implementations (`browser-harness`, and especially `browsercode`, which "works well today"). The job is to converge the browse half toward browsercode's model. The single most important insight: **most of the convergence is wiring pieces that already exist in the tree, not building new ones.**

## 2. The four reference points

| Repo | Role | Stack | One-liner |
|---|---|---|---|
| `repos/codex` | harness-core ideal | Rust | Codex CLI; new-core's harness already mirrors it. |
| `repos/browser-harness` | browse-interaction ideal (philosophy) | Python+CDP | Persistent CDP daemon; agent writes raw helpers; 97+ agent-authored domain-skills. No agent loop of its own. |
| `repos/browsercode` | the *working* combination | TS (opencode fork) | opencode harness + `browser_execute(code)` running JS **in-process** against a **persistent CDP session**. The reference to copy. |
| `repos/browser-harness-js` | JS CDP SDK blueprint | TS/Bun | 652 typed CDP wrappers + persistent `Session`; what browsercode vendored. Blueprint for a Rust `Session`. |
| `new-core/main` | what we're converging | Rust | Codex-faithful harness + multi-provider LLM + process-spawning CDP control plane. |

## 3. The five differences that define "like browsercode" (ranked)

1. **Execution substrate** — browsercode runs agent code **in-process** (JS `AsyncFunction`); new-core **spawns a fresh `python3` process + per-script TCP bridge + 3 threads per script `start`**. *(Headline gap.)*
2. **Tool surface** — browsercode = stateless `browser_execute(code)`; new-core = stateful `start → observe(poll) → cancel` protocol (a direct consequence of #1's detached process).
3. **Self-improvement memory** — browsercode/browser-harness let the agent **write** reusable helpers + site-specific domain-skills that compound across runs; new-core has the *read* side only. *(A missing capability, highest quality leverage.)*
4. **Raw-CDP-as-code** — references give the model raw CDP (`session.Page.navigate`); new-core gives a curated Python SDK (`goto_url`, `click_at_xy`…) with a `cdp()` escape hatch.
5. **Automatic vision** — browsercode taps `Page.captureScreenshot` results → next-turn image; new-core routes screenshots through files + a stdout marker, plus a separate serial `view_image` tool.

## 4. Verified facts — new-core browser stack (`crates/browser-use-browser`, `crates/browser-use-agent`)

**Tool adapter** (`browser-use-agent/src/tools/handlers/browser.rs`):
- Model-facing schema: `action ∈ {command,start,observe,cancel}` + `script`/`code`, `command`, `run_id`, `session_id`, `timeout_secs`, `observe_timeout_ms` (`:195-277`).
- `parallel_safe=false` (`:2064`); runs on `tokio::task::spawn_blocking` because the backend is sync (`:2161`).
- `start` returns `run_id` + `next_observe_ms`; model must poll `observe` until done, or `cancel`.

**CDP control plane** (`browser-use-browser/src/lib.rs`):
- `spawn_browser_script_with_session_registry` (`:970`): **per `start`** creates a TCP listener (`:983`), a bridge thread (`:994`), a **Python subprocess** (`:1021`) running a base64 prelude (`:6469`), + 2 reader threads (`:1031-1032`).
- Python selection: `LLM_BROWSER_BROWSER_SCRIPT_PYTHON` → `VIRTUAL_ENV`/`.venv` → `uv run` → `python3` (`:1430`).
- **CDP connection IS persistent**: `BrowserSession.connection: Option<Arc<CdpDispatcher>>` (`:185`) in a global registry (`:323`, `BROWSER_SESSIONS` OnceLock `:236`), keyed by session id, reused across calls.
- `CdpDispatcher` (`:3872-3961`): persistent WebSocket, request/response by id, **sessionId auto-injected per call** (`:3931`) — functionally identical to browsercode `session.ts:188`. Default 30s timeout.
- **The dispatcher loop drops events** (`:3979-4045`, events discarded at `:4026`). **No `onCallResult`/response tap.** ← the two missing primitives.
- Bridge is **stateless per request** (`:6019,6052,6081`): Python opens a socket, sends one-line JSON, gets one response, closes.
- 2fps background frame-capture thread tied to the subprocess lifecycle (`:6567`, dedup by Blake2b hash).
- Connection modes `BrowserMode`: Local / Managed (Rust-owned launch) / RemoteCdp / RemoteCloud.

**Python helpers** (`browser-use-browser/src/browser_script_helpers.py`): high-level SDK — `goto_url`, `page_info`, `click_at_xy`, `fill_input`, `press_key`, `screenshot`, `wait_for_network_idle`, `http_get`, plus raw `cdp(method, …)` (`:26`).

**Vision** (`browser-use-agent/src/tools/handlers/view_image.rs`): `view_image` is **sync + forced serial** (`parallel_safe=false` `:223`, blocking `std::fs::read`) by deliberate design (D-DIV-3) so images are observed in order. In-script `screenshot()` → JPEG file → `images[]` → `__browser_script_content__` stdout marker → vision `ContentPart`.

**Workspace/domain-skills — READ side already built, no authoring** (`browser-use-browser/src/lib.rs`):
- `agent_workspace_dir_for`, `domain_skill_roots_for` (4 roots: workspace `domain-skills`, state-dir, `~/.browser-use-terminal/agent-workspace/domain-skills`, project repo), `domain_skills_enabled()` (`:1508-1583`).
- `agent_helpers.py` auto-load (`:6884-6901`). Prompts expose `domain_skills_for_url(url)` / `last_domain_skills()`.
- **Missing:** any tool for the agent to *write* new helpers/domain-skills that persist to next run.

## 5. Verified facts — the warm runtime that already exists (`crates/browser-use-python-worker`)

This is the key enabler and is currently **unused by the browser stack**.
- **Long-lived persistent subprocess**, not per-call: `PythonWorker::start` (`lib.rs:76`), spawns `python -m llm_browser_worker.worker` (`:433`); killed only on `Drop` (`:491`).
- **Per-session namespace persists globals/imports across calls** — proven by its own test `worker_keeps_a_persistent_namespace_per_session` (`lib.rs:503-530`); namespaces dict keyed by `session_id` (`worker.py:27,621`).
- IPC: line-framed JSON over stdin/stdout (`lib.rs:318-372`); streams intermediate events `output`/`artifact`/`image`/`browser` before the final response (`worker.py:593-606`).
- **Already imports `browser_harness.admin`/`helpers`** into the namespace (`worker.py:521-550`).
- Used today only by the `python` tool (`browser-use-agent/src/tools/handlers/python.rs`, `parallel_safe=false`).
- Gotchas for hosting browser scripts: hard-restart on timeout kills *all* namespaces (`lib.rs:331`); per-call result buffers reset but user globals persist (`worker.py:637-640`); session-id↔browser-session mapping is undefined.

## 6. Verified facts — the references (for blueprint/comparison)

- **browsercode** `packages/bcode-browser/src/browser-execute.ts`: agent code via `new AsyncFunction("session","console", code)` invoked `wrapped(session, snippetConsole)` (`:128,166,222`); explicitly *"No subprocess, no daemon, no socket, no uv"* (`:3-6`). Screenshot auto-attach taps `session.onCallResult` filtering `Page.captureScreenshot` (`:204-219`) → FilePart `data:` URLs in the Level-2 adapter `packages/opencode/src/tool/browser-execute.ts:64-81`. One persistent `Session` per opencode-sessionID in `session-store.ts:22-28`. Model tool surface = `browser_execute(code, timeout?, description)`. Agent writes reusable `.ts` to `.bcode/agent-workspace/`, imports with `await import("…?t="+Date.now())`.
- **browser-harness** `src/browser_harness/daemon.py`: long-lived daemon multiplexes one CDP WS; thin subprocess client per call; 97+ agent-authored `domain-skills/<site>/`.
- **browser-harness-js** `sdk/session.ts:44-210`: `Session` class — one WS, sessionId auto-inject (`:188`), `onEvent`/`waitFor` (`:156-178`), 652 typed wrappers in `sdk/generated.ts`. This is the **blueprint for a Rust `Session`**, and `CdpDispatcher` is already its Rust transport equivalent.
- **cdp-use** (`repos/cdp-use`) is a **Python** library — NOT a Rust crate. Not a dependency candidate; only an architectural reference for typed CDP + event registration.
- **No JS-engine deps** (`deno_core`/`v8`/`boa`/`quickjs`) exist in `Cargo.lock` today.

## 7. Sanctioned divergences from Codex — DO NOT "fix" these (`DECISIONS.md`)

- D-DIV-1 Multi-provider (OpenAI Responses, Anthropic Messages + Claude-Code OAuth, Ollama, DeepSeek, OpenRouter, Fireworks) via a protocol×provider split (`browser-use-llm`).
- D-DIV-2 SQLite is a **write-only sink**, never hot-polled; read only on resume.
- D-DIV-3 `view_image` is **sync/serial** on purpose (ordered observation).
- D-DIV-4 Browser + Python tool surface is the product layer.
- D-DIV-5 ChatGPT/Codex backend dropped entirely.

## 8. The evidence that this direction is right (`docs/eval-journal/regression-88-to-81-analysis.md`)

- The 88→81 eval regression is caused by **per-call browser claim/release leases: 1854 claim + 1854 release events per 100-task run** vs **0** in the persistent-connection baseline; tool-failure rate ~64 → ~309 (5×).
- The doc's own north-star: *"move toward one persistent-session browser tool with synchronous execution and implicit/lightweight done — shed the runtime 'manager layer,' per browser-harness/browsercode."*
- Current branch `fix/runtime-terminal-barrier` is step 1 of that fix plan.
- **Conclusion:** the browsercode divergences and the eval regression have the *same* root cause. Converging fixes the regression.

## 9. Recommended path (and what to NOT do yet)

**Do:** persistent-warm-runtime + stateless-tool + agent-authored-memory, in this order:
- **Phase 0** — finish the in-flight regression fix (persistent connection, drop per-call leases). *Recovers ~6–8 eval points; prerequisite for the rest.*
- **Phase 1** — route browser snippets through the **existing warm `browser-use-python-worker`** with a **persistent bridge** to `CdpDispatcher`, instead of spawning a process per script. *(Closes difference #1.)*
- **Phase 2** — collapse the tool to stateless **`browser_execute(code)`** with implicit done; keep `observe/cancel` only behind `background:true`. *(Closes #2.)*
- **Phase 3** — add the **write path** for workspace/domain-skills (read side already wired). *(Closes #3; highest quality-per-effort; independent of #2.)*
- **Phase 4/5** — dispatcher **event subscription** (stop dropping events at `lib.rs:4026`) and an `onCallResult` **screenshot tap**. Small, optional polish. *(Closes #4 ergonomics / #5 vision.)*

**Don't (yet):** embed a JS engine for JS-as-CDP. It's the fullest "like browsercode," but it's greenfield (no JS-engine deps), already scoped as Phase 6 `code_mode` (`IMPLEMENTATION_PLAN.md:73`; `non_code_mode_only` is only a test stub `config_overrides.rs:74`), and Phases 1–3 deliver ~all the model-visible feel in Python. Pursue only if JS-as-CDP becomes a committed product goal.

Critical path: **0 → 1 → 2**. Phase 3 lands anytime after 1.

## 10. Phase-1 watch-outs (the hard parts)

1. **Session mapping** — worker namespaces are keyed by arbitrary `session_id`; browser scripts bind to one browser/CDP session. Define agent-session ↔ browser-session ↔ worker-namespace cleanly so two browser sessions can't collide.
2. **State-bleed policy** — worker persists user globals across calls. Decide: persist (browsercode-like, helpers accrete) vs. reset per call. Recommend persist helpers/imports, scope per-call result buffers (already reset at `worker.py:637-640`).
3. **Persistent bridge** — replace the per-script `BRIDGE_PORT` (`lib.rs:983`) with a long-lived handle from the worker to the live `CdpDispatcher`; `cdp()` keeps its signature, only its transport target changes.
4. **Frame capture** — re-home the 2fps capture (`lib.rs:6567`) from per-subprocess to per-browser-session, or drop it for snippet-driven `screenshot()` (browsercode has no frame thread).
5. **Cancellation** — need per-snippet cancel that does NOT hard-restart the worker (current timeout path nukes all namespaces, `lib.rs:331`).

## 11. Open decisions to confirm before building

1. **State persistence semantics** (P1): persist agent globals across snippets, or reset per call?
2. **Background escape hatch** (P2): keep `observe/cancel` for `background:true`, or browsercode's "just a 10-min timeout, no background"?
3. **Memory scope** (P3): per-project `.../agent-workspace/` (browsercode `.bcode/`) vs. per-user home as default. (Existing roots already include both — `lib.rs:1508-1583`.)
4. **Language stance** (P6): is Python-with-`cdp()` the long-term substrate, or is JS-as-CDP committed? *This is the only question that forces the big track.*

## 12. Guardrails for the implementing agent

- Project ethos: **extreme parity with Codex, no shortcuts** — match mechanism/heuristics, not just "feature exists." The browse-layer changes here are *sanctioned divergences*, but everything else stays codex-faithful and test-backed.
- **Tests first, commit first**; run-and-test against a live model, not compile-only.
- Don't touch the harness core, the multi-provider layer, or the D-DIV divergences unless a phase explicitly requires it.
- Re-verify every `file:line` before editing.
</content>
