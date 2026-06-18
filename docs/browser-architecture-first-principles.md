# Browser layer, from first principles

**Method:** independent deep-read of all four codebases (codex `core/src`, browser-harness py+js, browsercode `bcode-browser` + opencode glue, new-core), deliberately ignoring the prior convergence docs' conclusions. Eval journal used only as measured data (event counts, failure rates). Written 2026-06-09.

---

## 1. What each reference actually is (essence, not feature list)

**codex** is a streaming turn loop with in-flight tool execution, and — the part that matters here — it already has a native idiom for *long-running interactive runtimes*: **`unified_exec`** (`codex-rs/core/src/unified_exec/mod.rs`). A warm `ProcessStore` of persistent process handles, reused across tool calls, with `yield_time_ms` semantics (return partial output after a short wait; the model calls the tool again with the same process id to continue), head/tail output buffering, and token-budgeted truncation. Codex never invented a bespoke start/observe/cancel protocol for long work — the *process handle + yield* idiom is its answer.

**browser-harness** is a deliberately thin pass-through: one long-lived daemon holding one CDP WebSocket, an event ring buffer (500 events) so waits don't poll, silent stale-session re-attach (`daemon.py:352`), `flatten:true` sessionId routing, and a raw `cdp()` escape hatch as the *primary* interface — helpers are sugar. Its AGENTS.md states the doctrine outright: *"Don't add a manager layer. No retries framework, session manager, daemon supervisor, config system."* Domain skills are agent-**authored output** of runs, surfaced as filenames on `goto_url`.

**browsercode** is the existence proof that the combination needs almost nothing: opencode is untouched except one generic tool. `browser_execute(code, timeout?, description)` compiles the snippet via `AsyncFunction` in-process against a persistent per-session CDP `Session` (`bcode-browser/src/browser-execute.ts:128,166,222`). Synchronous, 60s default / 10min max, **no background mode, no polling**. Screenshots auto-collected by tapping `Page.captureScreenshot` results (`:204-219`) → image attachments next turn. Connection is **snippet-driven** (the agent calls `session.connect()` in code; the tool does zero provisioning). Reusable code is **plain files**: the agent uses the standard read/write/edit tools on `.bcode/agent-workspace/*.ts` and dynamic-imports them — no authoring tool, no registry. And critically: the instructions live in a **SKILL.md loaded on demand** (the tool description is ~7 lines saying "load the skill first"), not in the system prompt.

---

## 2. The real differences, ranked (my framing — differs from the prior docs)

### #1 — The manager-layer inversion (the headline; broader than "process spawn")

Both references are radically thin on the harness side of the browser. new-core inverted that:

- a 9.5k-line `browser-use-runtime` crate doing **per-call browser claim/release leases with synchronous SQLite barrier writes** (~3,700 barrier transactions per 100-task eval; baseline had 0),
- a **second model-facing tool** — `browser`, a ~50-subcommand control-plane CLI (connect/status/doctor/recover×5/profiles/skills…),
- a **diagnosis/recovery taxonomy** that classifies errors for the model and demonstrably mislabels ~100ms transient CDP timeouts as fatal "browser-closed", triggering recovery loops that burn 80 turns (eval task 26),
- a **done-audit + terminal-completion barrier** that caused measured failures (tasks 1, 4, 53, 94).

The eval regression (88→81, 5× tool-failure rate) is concentrated *in this layer*, not in Python-vs-JS or SDK-vs-raw-CDP. The per-script Python spawn (prior docs' headline) is real but is a *symptom* of the same instinct: making browser lifecycle a first-class harness concept instead of something agent code handles inside snippets.

**Principle violated:** the harness should know the browser exists only as "a tool that runs code." Lifecycle, connection, recovery = agent-code concerns (helpers), not harness protocol.

### #2 — Prompt-surface economics (not in the prior docs at all)

new-core front-loads **~6.3k tokens** of browser instruction into *every* prompt: `browser-tool-description.md` (~450) + `browser-script-tool-description.md` (~700) + `browser-agent-system.md` (~1,950) + 18 interaction-skills (~3,200). browsercode's equivalent permanent footprint is **~10 lines**; the full SKILL.md enters context once, on demand, via the skill tool. Two tools also impose a mode-selection burden (which tool? which of 50 subcommands? which action?) that one tool doesn't. This is a per-turn tax *and* a behavioral one — eval shows "script-over-visual flight" strategies (5 tasks) consistent with an over-instructed model.

### #3 — Ephemeral process vs persistent runtime

new-core spawns a fresh Python interpreter + per-script TCP bridge + 3 threads per `browser_script` call; state dies with the process (helpers must round-trip through files). browsercode/browser-harness hold a warm runtime where `globalThis`/namespace state survives across calls. The CDP WebSocket in new-core *is* already persistent (`Arc<CdpDispatcher>` registry) — the gap is purely the code runtime.

**The codex-idiomatic fix exists in codex itself:** a persistent Python REPL process per agent session, managed like a `unified_exec` session (warm handle in a ProcessStore, yield-based partial output for the rare long run). That one move is simultaneously codex parity *and* browsercode equivalence. The tree even has the piece built: `browser-use-python-worker` is a long-lived worker with persistent per-session namespaces, currently used only by the `python` tool.

### #4 — Start/observe/cancel vs synchronous-with-timeout

A direct consequence of #3. browsercode: snippet runs to completion, 60s default, 10min max, *no* background mode — and it works on the same eval class. new-core: `start` returns after 750ms with a `run_id`, model polls `observe` (min clamp **30s** per poll — measured wall-clock amplification, tasks 1, 4 never finished). When the runtime is warm and synchronous, the whole protocol collapses; for the genuinely-long tail, codex's `unified_exec` yield/poll idiom is the parity-correct escape hatch — not a bespoke browser protocol.

### #5 — Error doctrine: surface raw vs diagnose-and-orchestrate

browser-harness does exactly **one** silent auto-heal — stale-session re-attach on `"Session with given id not found"` (`daemon.py:352-356`) — and otherwise returns raw CDP errors and lets the agent decide. new-core's five recovery commands + `browser_usable:false` diagnosis is a retries-framework by another name, and it's the proximate cause of the worst eval thrash. Raw error text + one cheap auto-heal beats a taxonomy.

### #6 — Knowledge accretion is a *convention*, not a feature

browsercode ships **zero** authoring machinery: the agent writes plain `.ts` files with the standard file tools into a well-known dir, imports with cache-bust; SKILL.md teaches the pattern. browser-harness: same, plus 97 agent-authored domain-skill dirs surfaced as filenames by `goto_url`. new-core already has the read side (`agent_workspace_dir_for`, `domain_skills_for_url`, `agent_helpers.py` auto-load) — what's missing is **prompt doctrine + a writable convention**, not a new tool. The prior docs framed this as missing machinery; it's mostly a SKILL.md paragraph plus making sure the workspace path is stable, writable, and auto-loaded.

### #7 — Vision plumbing (minor)

browsercode taps `Page.captureScreenshot` results in-process → attachments. new-core routes through JPEG files + a stdout marker; it works, and the serial `view_image` (codex parity, D-DIV-3) is fine to keep. Worth simplifying only after the dispatcher gets a response tap; not a driver.

### Where new-core is genuinely ahead (do not regress)

- Codex-faithful harness core: real per-provider token accounting, multi-provider protocol×provider split, orchestrator/approval seam.
- Persistent `CdpDispatcher` with sessionId auto-injection — functionally browsercode's `Session` transport already.
- Connection modes (Local/Managed/RemoteCdp/RemoteCloud) — broader than any reference.
- Eval-proven behaviors from the rewrite: API-first structured extraction (fixed 8 tasks), partial-output resilience, audit *discipline* (the idea, with fixed thresholds — not the current rejection behavior).
- Event-sourced SQLite durability (as a write-behind sink, per D-DIV-2 — the problem is per-call *barrier* writes, not durability itself).

---

## 3. Target architecture (one page)

**The model sees:**
- One tool: **`browser_execute(code, timeout_secs?, description)`** — ~10-line description ending "load the browser skill before first use."
- A **browser SKILL.md** (on-demand): connection ways (snippet-driven `connect_local()` / `connect_managed()` / `connect_cloud()` helpers), the helper API, raw `cdp()` doctrine, screenshot/vision contract, workspace + domain-skills conventions, error-recovery-as-code recipes. The 18 interaction-skills become referenced files the agent reads when stuck, not prompt preamble.
- `view_image` unchanged (codex parity).
- No `browser` control-plane tool. Profiles/doctor/status become helpers callable inside snippets; anything user-facing moves to TUI slash-commands, off the model's token budget.

**Under the hood:**
- Per agent session: one warm **Python REPL process** (evolve `browser-use-python-worker`) holding the helper namespace + a **persistent bridge** to the already-persistent `CdpDispatcher`. Namespace persists across calls (imports, helper defs, variables). One process per session; killed on session end.
- `browser_execute` = "run snippet in the warm namespace, synchronously, default 60s / max 10min." Long-tail escape hatch: the **`unified_exec` idiom** — return a session handle + partial output at yield time; the model re-calls to continue. No bespoke observe/cancel verbs.
- **Browser lease = session-lifetime**, claimed on first connect, released on session end. Durability events for browser activity demoted from `Barrier` to async journal writes.
- Error doctrine: raw error text into the tool result; one silent stale-session re-attach in the dispatcher (port `daemon.py:352`); delete the recovery-command taxonomy (recipes live in SKILL.md as *code* the agent can run).
- Dispatcher gains an **event ring buffer** (≈500 events, per browser-harness) so `wait_for_network_idle`/event-waits stop polling, and (later) a response tap for automatic screenshot attach.
- Workspace: keep current roots; SKILL.md teaches write-helpers-as-files + `load_agent_helpers()`; `goto_url` keeps surfacing domain-skill filenames; agent authors skills with ordinary file tools.
- done-audit: keep the audit *pass* but fix thresholds to distinguish legitimately-unavailable from missing (eval H5), and land the terminal-barrier fix (in flight).

**Language stance:** stay Python-with-raw-`cdp()`. Both references prove the load-bearing element is *persistent runtime + raw protocol + accreting helpers*, not JavaScript per se. Embedding V8 buys philosophy points at enormous cost; revisit only if evals show Python itself (not the plumbing) is the limiter.

---

## 4. The plan

Ordered by measured-eval leverage per unit risk. Each phase independently shippable + evaluable.

**P0 — land the in-flight fixes (current branch).** Terminal-completion barrier (tasks 1, 4), transient-vs-fatal error classes (task 26 class), revert the 30s observe min-clamp, audit threshold fix (tasks 53, 94). *Expected: recovers most of the 7-point regression; prerequisite stability for everything below.*

**P1 — session-lifetime browser lease + barrier demotion.** Claim on connect, release on session end; browser activity events become async journal writes. Removes ~3,700 sync barrier writes/run and the false "already in use" surface. Small, isolated to `browser-use-runtime` + the browser handler seam.

**P2 — warm runtime.** Browser snippets execute in the per-session persistent Python worker with a persistent bridge to `CdpDispatcher`. Namespace persists. Watch-outs: per-snippet cancel must not nuke the worker (current timeout path hard-restarts it); map agent-session↔browser-session↔namespace explicitly; re-home the 2fps frame-capture thread per-session or drop it.

**P3 — collapse the model surface.** One `browser_execute`, synchronous default, unified_exec-style handle for the long tail; delete the `browser` control-plane tool (helpers absorb it); move ~6.3k prompt tokens into an on-demand SKILL.md; tool description to ~10 lines.

**P4 — error doctrine + dispatcher primitives.** Stale-session auto-reattach; event ring buffer; raw-error pass-through replacing the diagnosis taxonomy; SKILL.md recovery recipes.

**P5 — accretion doctrine.** SKILL.md sections for write-your-own-helpers + file-a-domain-skill; verify auto-load and `goto_url` surfacing end-to-end; seed 2–3 domain skills by running real tasks (never hand-author — browser-harness rule).

**P6 (optional, later) — vision tap.** Dispatcher response tap → automatic screenshot attach; retire the file+marker dance. Keep serial `view_image`.

Phases are file-disjoint enough to parallelize: P1 (runtime crate) ∥ P2 (worker+browser crates) after P0; P3 depends on P2; P4/P5 independent after P2.

---

## 5. Explicitly rejected

- **Embedding V8/deno_core for JS-as-CDP** — cost/benefit fails while Python plumbing is the actual limiter.
- **Keeping the `browser` control-plane tool "for power users"** — it's the manager-layer inversion in tool form; helpers + TUI commands cover it.
- **Auto-recovery orchestration** — one silent re-attach max; everything else is agent code.
- **A skill/helper authoring tool or registry** — plain files + conventions, per both references.
- **Per-call durability barriers for browser activity** — durability stays (write-behind), barriers go.

## 6. Open product decisions

1. **Long-tail semantics in P3:** pure browsercode (hard 10-min cap, nothing survives the call) vs unified_exec handle (codex parity, model can continue a long run). Recommendation: unified_exec handle — it's codex-native and strictly more capable; the *default* stays synchronous.
2. **Workspace default scope:** per-project (browsercode `.bcode/`, git-tracked, shareable) vs per-user home (current default root). Recommendation: per-project default with home fallback.
3. **Frame-capture thread:** keep (re-homed per-session, observability win) or drop (browsercode has none; less complexity). Recommendation: keep behind a flag, off by default in evals.
