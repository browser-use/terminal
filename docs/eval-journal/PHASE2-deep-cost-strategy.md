# Phase 2 — Deep cost strategy (evidence-based)

All numbers from real run `real-v8-phase1merged-20260610-234722` (gpt-5.5, 100 tasks, 98 pass).

## The cost shape — it's input replay, not output

- **Input tokens = 112.5M. Output = 1.02M. Ratio 110:1.** Cost is almost entirely the context that gets
  **replayed every turn**, not what the model writes.
- Of the 112.5M input bill:
  - **~62M (55%) is page content** (tool output: HTML/DOM/scraped text) replayed across turns.
  - **~44M is fixed boilerplate** (system prompt 8,135 tok + tool descriptions 11,077 tok = **19,212 tok/turn** × ~23 turns × 100 tasks). **BUT this is ~99% prompt-cached** (100 tasks share the same prefix; first task warms it, rest read cached at ~0.1×). So prompt-cutting saves cache-reads, not full-price tokens — modest $.
  - The rest is conversation/reasoning.
- **Caching reframes priorities.** Real $ ≈ (unique tokens at full price) + (replayed tokens at cache-read price) + output. The unique-content full-price portion ≈ the sum of final contexts ≈ 6.6M. Everything else is cache-reads. ⇒ **Track A (cost meter) MUST split cached vs uncached** or we'll optimize the wrong thing. (The data is already in `token_count.info.*`: `input_tokens`, `cached_input_tokens`, `cache_creation`, `output_tokens`, `reasoning_output_tokens`.)

## The three levers, ranked by real leverage

### Lever 1 — fewer TURNS (highest confidence, helps score too)
- **19.3% of all model calls are pure overhead** (no task progress) ≈ 8.9M tokens, and each is a full
  context cache-read of 20–70k tokens.
- #1 cause: **`browser_script` is async** — model `start`s, then must spend separate `observe` *model
  turns* to fetch the result. 185 wasted calls (~3.7M tok) on observe/re-observe churn. #3: passive
  `browser status --json` polling = 63 calls (57 from task 6 alone, which ran out of turns and FAILED).
- **Fix = make browser_script result retrieval synchronous** (block until done within a deadline; one
  long-poll observe for genuinely long runs instead of returning "still running" as a model turn).
  Pure harness change, no parity/prompt risk, and it *frees turns* (helps tasks like 6/21/26 that ran out).
- Tension: the 1s window was restored for the 88-baseline because a coarse 30s window left tasks
  unfinished — so the fix is "return as soon as done, long-poll otherwise," not a fixed bigger window.

### Lever 2 — less page-HTML in the EXPENSIVE model's context (biggest token pool)
- Page content = 55% of the input bill. `browser_script` outputs = 90% of tool-output bytes. **15 outputs
  pegged at the 120KB cap (~30.8k tokens each)**; 6.4% of outputs (>20KB) carry 56% of page-content bytes.
- We cap browser_script stdout at **120KB ≈ 30.8k tok**; **codex caps observations at 10k tok** with
  **head+tail middle-elision** (`…N tokens truncated…` + `Total output lines: N` banner). We're 3× codex
  and we keep the whole thing.
- Two sub-levers:
  - **(2a) codex-style truncation** to the EXPENSIVE model: head+tail elision ~10k tok + image-aware
    token estimate (count a screenshot as a flat ~1.8k tok, not its base64 length). Deterministic, cheap.
    **Risk:** re-introduces the blindfold we just fixed — UNLESS paired with 2b (below).
  - **(2b) offload heavy reading to a CHEAP model** that returns ≤2KB distilled. Removes **~42% of the
    main input bill (47M tok)**; pure-scrape tasks (27, 40, 38, 15, 60, 59) drop **70–77%**.
- **The tension resolves elegantly when 2a+2b combine:** give the FULL raw text to the CHEAP extractor;
  give only the distilled result (or a truncated head+tail) to the EXPENSIVE planner. The 120KB cap
  existed because the *expensive* model coded against truncated data — if it never needs the raw dump,
  the cap stops mattering.

### Lever 3 — smaller fixed prompt (low-risk, but mostly cached ⇒ modest $)
- Fixed = 19,212 tok/turn. Safe cuts ≈ **−7,650 (−40%)**: trim browser_script desc (−1,900), drop 12
  interaction-skill stubs + screenshots.md (−1,300), trim browser tool desc (−1,200), **gate the 6
  multi-agent tool descriptions when delegation isn't active (−2,700)**, compress update_goal (−350).
- The two browser tool descriptions are 56% of the tool budget and restate the system contract 3×.
- Caveat: this is largely cache-read, so $ savings are real but smaller than the token count implies; the
  win is also a smaller context window + cheaper cold first-turn. Do it, but don't expect it to move $ much.

## Sub-agents: what's ready, what's broken, and the right shape

**Our sub-agent system is a near-complete codex port and the control plane is READY:**
- `spawn_agent` (+ `wait_agent`/`send_message`/`close_agent`/`list_agents`/`followup_task`) live-wired
  (`subagents/`, ~4.7k LOC). Confirmed against codex — ours mirrors theirs (same tools, fork modes, caps).
- **Context forking works**: `fork_turns` = none (fresh) / all / N. Result returns as a distilled summary.
- **Per-spawn model override works** (`model` + `reasoning_effort`), validated against the catalog.
  Cheap in-catalog model today = **`gpt-5.4-mini`**. Gemini-flash is a wired provider but needs a
  `model_catalog_json` override to be spawnable from a gpt-5.x parent.

**The one broken piece (your instinct was right): browser/CDP sharing.**
- Every agent eagerly creates its OWN browser (`create_browser_for_agent` mints a fresh `BrowserId`;
  child session id is distinct). A child CANNOT attach to the parent's live page. A test even asserts
  cross-session isolation. The `claim_browser`/`with_browser_action` lease API is the seam, but nothing
  wires a child to the parent's `browser_id`.
- Codex gives no parity help here — codex has no browser. This sharing is OUR invention to build.

**Therefore split the vision into two sub-agent patterns:**

| pattern | needs shared CDP? | status | win |
|---|---|---|---|
| **B1 — extraction/distillation agent**: receives raw text (HTML dump) as `input_items`, returns ≤2KB structured data. Never touches the browser. | **No** | **Buildable today** (spawn fork:none + `gpt-5.4-mini` + pass HTML; or a dedicated cheap `extract` tool) | captures most of the 42% — the cost is HTML *sitting in context*, not the interaction |
| **B2 — interaction agent**: drives the page (click/fill/scroll) on the planner's session. | **Yes** | **Blocked** on the browser-lease rewire | removes interaction-turn churn; higher effort, accuracy-risky with cheap models |

**Recommendation: do B1 first.** It's unblocked, captures ~most of finding-3's 42%, and it *also*
resolves the truncation/blindfold tension (cheap agent sees full text, planner sees the distillate).
B2 is the harder, browser-rewire bet — do it after B1 proves cheap-model extraction quality holds on the
scrape-heavy tasks.

## Codex ideas worth stealing (cited)
1. **Head+tail middle-elision truncation** of big observations, token-budgeted, with a size banner
   (`utils/string/src/truncate.rs`, `output-truncation`). Default 10k tok/observation.
2. **Image-aware byte→token estimate** (subtract base64, add flat ~1.8k/image; `history.rs:525`).
3. **Auto-compaction on a window-% trigger** with a structured handoff prompt (`compact.rs`, `templates/compact/prompt.md`).
4. **Record-time truncation with ×1.2 serialization budget** + **strip orphans/unusable images every prompt build** (`history.rs:366,378`).
   (Codex does NOT use a cheaper model for sub-tasks — its only downshift is the low-effort `awaiter` role. The cheap-model-extractor is our own idea.)

## Recommended sequence (cost-first, each its own worktree+PR, gated on eval+judge)
1. **Track A — cost meter** (½ day, low risk): wire live-run $ split cached/uncached/output/reasoning. Prereq for proving everything.
2. **Lever 1 — sync browser_script** (harness, low risk): kill observe/status churn. Re-judge: expect score ≥93 (may rise) + fewer turns.
3. **Lever 3 — prompt trim −40%** (low risk): free context + cheaper cold turns.
4. **Lever 2a + B1 — truncate-to-planner + cheap extractor** (the big token pool): full text → cheap extractor → distillate; head+tail cap to planner. Validate cheap-model quality on tasks 27/40/38/15/60/59.
5. **Track C — search (PR #67) + fetch-use in python**: un-blockable I/O kills retry/give-up turns (21/26/29).
6. **B2 — shared-CDP interaction agent** (highest effort): rewire child browser-backend to claim parent browser via the lease API + action-level serialization. Only after B1 validates.

## Open risks / unknowns
- Cheap-model (gpt-5.4-mini / gemini-flash) extraction accuracy on messy DOM — the whole B1/B2 bet rides on this; validate early on the scrape-heavy tasks.
- Truncation cap must be paired with B1 or it re-blindfolds the planner (the regression we just fixed).
- B2 needs action-level locking on one CDP connection (two agents can't drive the same page concurrently).
- $ figures pending Track A; gpt-5.5 is via codex OAuth so price-per-token must come from the pricing table, and caching makes prompt-cut $ smaller than token counts suggest.
