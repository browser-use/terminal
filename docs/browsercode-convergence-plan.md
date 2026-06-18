# Making new-core's browser layer like browsercode — convergence plan

Companion to [`codex-browser-harness-differences.md`](./codex-browser-harness-differences.md). That doc establishes *what* differs; this one is *the steps*. It is grounded in a feasibility sweep of what already exists in the tree, so the steps are framed as "wire together / extend what's there," not "build from scratch."

## TL;DR recommendation

**Take the "persistent warm runtime + stateless tool + agent-authored memory" path — not the embed-a-JS-engine path.** Everything needed for the first three phases **already exists in the tree**; the work is wiring, not greenfield. The JS-as-CDP route (embed V8) is a real but *separate, larger* track (Phase 6 `code_mode` in `IMPLEMENTATION_PLAN.md:73`) and is not how you get "much more like browsercode" soonest.

Crucially, this plan **subsumes and extends the in-flight regression fix** (`fix/runtime-terminal-barrier`, the current branch) and the fix plan already written in `docs/eval-journal/regression-88-to-81-analysis.md`. Those steps (persistent connection, drop per-call leases, implicit done) *are* the first half of browsercode convergence. This doc connects them to the end state.

---

## What browsercode's "it just works" decomposes into (the target)

From the differences doc, ranked by leverage:

1. **Persistent in-process execution** — no per-script process spawn; a warm runtime holds the session.
2. **Stateless tool surface** — one `browser_execute(code)`; snippet runs synchronously to completion; implicit/lightweight done. No `start/observe/cancel` polling.
3. **Self-improvement memory** — agent writes reusable helpers + site-specific domain-skills that compound across runs.
4. **Raw-CDP-as-code** — agent drives `session.X.y(...)` directly (philosophical; Python `cdp()` escape-hatch already exists).
5. **Automatic vision** — `captureScreenshot` inside a snippet auto-attaches as a model image next turn.

## What already exists (the feasibility findings)

| Building block | Where | State |
|---|---|---|
| Persistent warm Python interpreter, **per-session namespace persists globals/imports** | `browser-use-python-worker` (`lib.rs:16,76,426`; namespace `worker.py:27,621`; test `lib.rs:503-530`) | **Exists, used by `python` tool** |
| Worker already imports `browser_harness.admin`/`helpers` | `worker.py:521-550` | **Exists** |
| Streaming events over the worker IPC (`output`/`image`/`artifact`/`browser`) | `lib.rs:68-73,328-372`; `worker.py:593-606` | **Exists** |
| Persistent CDP transport with sessionId auto-injection | `CdpDispatcher` `browser-use-browser/lib.rs:3872-3961` (inject at `:3931`) | **Exists** |
| Per-session persistent CDP connection (`Arc<CdpDispatcher>`) | `BrowserSession.connection` `lib.rs:185`, registry `lib.rs:323` | **Exists** |
| Workspace + domain-skills **read** path (`agent_workspace_dir_for`, `domain_skill_roots_for`, `domain_skills_for_url`) | `lib.rs:1508-1583`; prompts | **Read-only; no authoring** |
| Agent-helpers auto-load (`agent_helpers.py`) | `lib.rs:6884-6901` | **Read-only** |
| North-star + regression evidence | `docs/eval-journal/regression-88-to-81-analysis.md` | **Documented** |

### What's genuinely missing (the actual work)
- The browser stack **spawns a fresh `python3` per script `start`** (`lib.rs:970,1021`) with a **per-script TCP bridge** (`lib.rs:983,994`) — instead of routing through the warm worker. **(Phase 1)**
- The tool is a **stateful `start/observe/cancel` protocol** (`tools/handlers/browser.rs:195-277`) because the script lives in a detached process. **(Phase 2)**
- **No agent-authoring surface** for workspace/domain-skills — agents can read accrued knowledge but not write it. **(Phase 3)**
- `CdpDispatcher` loop **drops events** (`lib.rs:4026`) and has **no response tap** — blocks event-wait and screenshot auto-attach without round-tripping through Python. **(Phase 4/5)**

---

## The plan — phased, ordered by leverage and risk

Each phase is independently shippable and improves evals on its own. Phases 1–2 are the "looks like browsercode to the model" core; Phase 3 is the highest *quality-compounding* leverage; Phases 4–5 are polish/optional; Phase 6 is the long-horizon JS track.

### Phase 0 — Land the in-flight regression fix (already underway)
**Goal:** finish `fix/runtime-terminal-barrier` and the `regression-88-to-81` fix steps 1–5 (persistent connection, **drop per-call browser claim/release leases**, retry transient provider errors, fix done audit, stop fatal-izing transient errors).
**Closes:** the per-call lease churn (1854→0 events) that is the structural cause of the current regression and is *itself* a browsercode-divergence.
**Why first:** it's in progress, it's the prerequisite "one persistent connection" assumption that Phases 1–2 build on, and it recovers ~6–8 eval points on its own.
**Acceptance:** lease event count ≈ 0 per run; tool-failure rate back toward baseline (~64, not ~309).

### Phase 1 — Route browser scripts through the warm worker (kill the per-script spawn)
**Goal:** stop spawning a fresh Python process + per-script TCP bridge per `start`. Run browser snippets in the **existing `browser-use-python-worker`** with a **persistent bridge** to the session's `CdpDispatcher`.
**Closes difference #1 (execution substrate)** — the headline gap.

Concretely:
- Give the worker a **persistent bridge endpoint** to the live `CdpDispatcher` (a long-lived socket/handle), set once when the browser session connects, instead of a new `BRIDGE_PORT` per script (`lib.rs:983`, prelude `lib.rs:6469`). The `cdp()` helper (`browser_script_helpers.py:26`) keeps the same signature; only its transport target changes from per-script bridge → persistent worker-held bridge.
- Load `browser_script_helpers.py` **once per session namespace** in the worker (it already loads `browser_harness`; `worker.py:521-550`) instead of base64-injecting the prelude every call.
- Reuse the worker's existing streaming events (`output`/`image`/`artifact`/`browser`) for incremental output — no new IPC.

**Watch-outs (from the feasibility dive):**
- **Session mapping.** Worker namespaces are keyed by an arbitrary `session_id`; browser scripts are tied to one *browser* session/CDP connection. Define a clean mapping (agent-session ↔ browser-session ↔ worker-namespace) so two browser sessions don't collide. (Per-namespace reset of `images/outputs/artifacts` already exists at `worker.py:637-640`.)
- **State bleed.** The worker intentionally persists user globals across calls. Decide: persist (browsercode-like, helpers accrete) vs. reset user vars per call. Recommend persisting helper/imports, scoping per-call result buffers (already reset).
- **Frame capture.** The per-script 2fps capture thread (`lib.rs:6567`) is currently tied to the subprocess lifecycle. Re-home it as a per-session capture keyed by browser-session, or drop it for snippet-driven `screenshot()` (browsercode has no frame thread).
- **Timeout/kill.** Worker hard-restart on timeout kills *all* namespaces (`lib.rs:331`). Need per-snippet cancellation that doesn't nuke the warm worker.

**Acceptance:** a browser snippet runs with **no new OS process** per call; CDP round-trips go warm-worker → persistent bridge → `CdpDispatcher`; latency per `start` drops to ≈ worker-call latency.

### Phase 2 — Collapse the tool surface to stateless `browser_execute(code)`
**Goal:** once snippets run synchronously in the warm worker, retire the `start/observe/cancel` run protocol for the common case. Model writes a snippet; it runs to completion; output (incl. screenshots) returns in one tool result. **Implicit/lightweight done.**
**Closes difference #2 (tool glue)** — directly matches browsercode's `browser_execute(code, timeout?, description)` (`tool/browser-execute.ts:23,56`).

Concretely:
- New model-facing surface: `browser_execute(code, timeout?, description)` in `tools/handlers/browser.rs`. Default action becomes "run snippet synchronously."
- Keep `observe`/`cancel` only as an **opt-in `background:true` escape hatch** for genuinely long-running tasks (browsercode clamps to a 10-min timeout instead; `browser-execute.ts:49`). Most tasks never touch it.
- Preserve `parallel_safe=false` and `spawn_blocking` (`browser.rs:2064,2161`) — serialization stays.

**Acceptance:** typical browser tasks complete in a single tool call with no polling; the `start→observe×N` transcript pattern disappears from eval logs.

### Phase 3 — Agent-authored memory: workspace + domain-skills write path
**Goal:** let the agent **write** reusable helpers and site-specific domain-skills that auto-load next run. The *read* side already exists (`agent_workspace_dir_for`, `domain_skill_roots_for`, `domain_skills_for_url`, `agent_helpers.py` load — `lib.rs:1508-1583,6884-6901`); only the authoring surface is missing.
**Closes difference #3 (self-improvement)** — the single highest *quality-compounding* leverage; this is much of *why* browser-harness/browsercode "work well" (97+ agent-authored domain-skills).

Concretely:
- A tool/helper for the agent to persist a `.py` (or `.md`) artifact into `agent_workspace_dir`/`domain-skills/<host>/` — mirroring browsercode's `.bcode/agent-workspace/*.ts` + `await import(...?t=...)` and browser-harness's `domain-skills/<site>/`.
- On `goto_url`, the existing `domain_skills_for_url` surfacing (already wired, `BH_DOMAIN_SKILLS`) now reads agent-authored entries too.
- Decide persistence scope: per-project (browsercode `.bcode/`, tracked) vs. per-user home (`~/.browser-use-terminal/agent-workspace`, already a root at `lib.rs:1508-1583`). Recommend per-project default + home fallback, matching the existing root precedence.

**Acceptance:** agent writes a selector/helper for site X in run A; run B on site X auto-loads it; measurable improvement on repeat-site eval tasks.

### Phase 4 — Event subscription in the dispatcher (unlock event-waits without Python round-trips)
**Goal:** make `CdpDispatcher` deliver events to subscribers instead of dropping them (`lib.rs:4026`), so `wait_for_load`/`waitFor("Page.loadEventFired")`-style waits and network-idle become first-class — closer to browser-harness-js `session.onEvent`/`waitFor` (`session.ts:156-178`).
**Closes:** part of difference #4 (raw-CDP ergonomics) and removes polling-based waits.
**Effort:** small — add a listener registry and dispatch before discarding events in `cdp_dispatcher_loop` (`lib.rs:3979-4045`).

### Phase 5 — Automatic screenshot vision via a response tap (optional)
**Goal:** add an `onCallResult`-style tap to the dispatcher so `Page.captureScreenshot` results auto-attach as model images, matching browsercode (`browser-execute.ts:204-219` → FilePart `tool/browser-execute.ts:64-81`).
**Closes difference #5 (vision).** *Lower priority:* new-core's current file+`__browser_script_content__`-marker path already delivers screenshots-as-vision; this just removes the file/marker dance. Keep the deliberate serial `view_image` (D-DIV-3) as the explicit ordered-observation tool.
**Effort:** small once Phase 4's tap infra exists.

### Phase 6 — (Separate, larger track) JS-as-CDP / `code_mode`
**Goal:** the fullest "like browsercode" — agent writes **JavaScript** against an in-process Rust `Session`, raw CDP, near-zero abstraction.
**Status:** greenfield. `code_mode (V8 tools-as-code)` is already scoped as Phase 6 in `IMPLEMENTATION_PLAN.md:73` / `REARCHITECTURE.md:120`; `non_code_mode_only` is only a test stub today (`config_overrides.rs:74`). Requires embedding `deno_core`/V8 (no JS-engine deps in the lock today) plus Bun-ism shims if reusing `browser-harness-js`.
**Recommendation:** **defer.** Phases 1–3 deliver ~all the model-visible browsercode feel with Python. Pursue JS-as-CDP only if you specifically want the raw-CDP-JS philosophy and are ready for the engine-embedding cost. The `Session` class in `repos/browser-harness-js/sdk/session.ts:44-210` is the blueprint; `CdpDispatcher` is already its Rust transport equivalent.

---

## Sequencing & dependencies

```
Phase 0 (in flight) ──► Phase 1 (warm worker) ──► Phase 2 (stateless tool)
                                                      │
Phase 3 (authoring) ──────────────────────────────────┤ (independent; can land anytime after P1)
                                                      │
Phase 4 (event sub) ──► Phase 5 (screenshot tap)      │
                                                      ▼
                                          Phase 6 (JS-as-CDP) — separate track, defer
```

- **0 → 1 → 2** is the critical path to "looks like browsercode."
- **3** is independent of 2 and arguably the best quality-per-effort; land it as soon as 1 stabilizes the runtime.
- **4 → 5** are small and can slot in opportunistically.
- **6** is optional and large; only if you want JS specifically.

## Why this ordering, in one line
Per-call leases and per-script spawns are *both* the current eval regression's cause *and* the browsercode divergence — so fixing them (Phases 0–1) pays for itself immediately, and Phase 2 then collapses the tool the model sees. Phase 3 makes the agent get better at every site over time, which is the compounding advantage the reference implementations actually have.

## Open decisions to confirm before building
1. **State persistence semantics** (Phase 1): persist agent globals across snippets (browsercode-like) or reset per call? (Recommend: persist helpers/imports, scope result buffers — already reset.)
2. **Background escape hatch** (Phase 2): keep `observe/cancel` for `background:true`, or follow browsercode's "just a 10-min timeout, no background" model?
3. **Memory scope** (Phase 3): per-project `.../agent-workspace/` (browsercode `.bcode/`) vs. per-user home as default.
4. **Language stance** (Phase 6): is Python-with-`cdp()` the long-term substrate, or is JS-as-CDP a committed goal? This is the only question that forces the big track.
</content>
