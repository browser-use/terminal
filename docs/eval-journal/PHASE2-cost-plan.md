# Phase 2 — Cost optimization (hold performance)

Goal today: **(1) cut $ cost, (2) keep ~93 score.** Evidence-based; every lever tied to real run data from `real-v8-phase1merged-20260610-234722`.

## The cost picture (where the money actually goes)

Cost is **not** dominated by prompt size. It's dominated by **number of model calls** — because
every turn replays the full context (10k–40k input tokens at gpt-5.5), and that replay grows as the
task goes on. So **each wasted turn is super-linear in cost.**

### Lever 1 (BIGGEST): browser-script poll-per-model-call — `browser.rs:74`, `browser-browser/lib.rs:32`
`DEFAULT_OBSERVE_TIMEOUT_MS = 1_000`. The observe handler waits ≤1s, and if the script isn't done
returns *"browser_script is still running. Next step: call observe again"* (`browser.rs:2513-2530`).
The handler does **not** loop — control returns to the model, which burns a **full model call** to
re-poll. Same pattern for passive `browser status --json` checks.

Measured churn (each `model.turn.request` == one model call; verified 1:1 with `tool.started`):

| task | model calls | observe polls | `browser status` polls | poll-like share |
|------|-------------|---------------|------------------------|-----------------|
| 6    | 80          | 7             | 57                     | **~80%**        |
| 81   | 20          | 8             | 1                      | ~40%            |
| 21   | 23          | 4             | 2                      | ~26%            |

Run-wide: 100 sessions → 2302 model calls; `browser_script.started`=1355 / `.completed`=1441 →
scripts routinely return "running" and get re-observed. Task 6 ran *out of turns* (80) on polling and
**failed** — so this lever is cost **and** score.

**Tension (important):** the 1s window was deliberately *restored* for the 88-baseline
(`browser.rs:67-74`) because a coarse 30s window "left tasks unfinished." So the fix is **not** just a
bigger number — it's letting one logical script-run return its result in one tool call while keeping
the model's ability to course-correct. Design options in Track B; validate by A/B with cost instrumented.

### Lever 2: getting blocked → retry/give-up turns — tasks 21, 26, 29
Native python `requests`/`urllib` and DuckDuckGo get blocked/rate-limited → the agent burns turns
retrying or gives up mid-crawl (21 all-null rates, 26 50/687 stores, 29 4 packages). Replacing the
fetch + search primitives with un-blockable ones removes those wasted turns → less cost **and** higher score.

## Measurement: can we track $ today? (almost — one small gap)

All token data is **already captured and persisted per-turn**: `token_count` events in `state.db`
(`events/map.rs:180`) carry `input_tokens`, `cached_input_tokens`, `cache_creation`, `output_tokens`,
`reasoning_output_tokens`. A full pricing engine already exists (`ModelPricing` + `add_usage_cost` +
LiteLLM price table, `browser-use-providers/lib.rs:7493-7670`).

**Why $ shows 0 for live runs:** the aggregator `usage_summary_from_events` (`cli/main.rs:9224`) keys
on `model.usage` events — which the **live runtime never emits** (it emits `token_count`).

**Smallest fix (~half day):**
1. add `model` to the `token_count` payload (`events/map.rs:180`) — the one genuine data gap (cost = tokens × per-model price);
2. point `usage_summary_from_events` at `token_count`, folding per-turn `last_token_usage` deltas through the existing pricing table.
The `cost_status` rollup in `usage_summary_from_manifest` already consumes the result — no schema change beyond #1.

## The two new tools — where they go

| tool | placement | why |
|------|-----------|-----|
| **search** | dedicated **LLM-level tool** (PR #67, already built) | `search.browser-use.com` (Parallel.ai proxy), auth = existing `BROWSER_USE_API_KEY`. Replaces ddg **and** removes hosted `web_search`. Returns `{title,url,published_date,content}`, token-truncated. Net **−227 lines**. Discrete decision point → first-class tool is right. |
| **fetch** | inside the **python sandbox** (`fetch-use` SDK) | It *is* a python SDK (`from fetch_use import fetch_sync`). Server does Chrome-TLS-fingerprint + proxy-by-country + session IP/cookie persistence so we don't get blocked. Model writes python calling `fetch`, never raw `requests`/`urllib`. Same key. |

Matches the instinct: **search separate (LLM tool), fetch inside python.** fetch-use has *no* search
function, confirming they're genuinely separate concerns.

## Plan (cost-first, three tracks, each its own worktree+PR on main)

- **Track A — MEASURE (do first, ~½ day, low risk).** Wire live-run $ (the 2-step fix above). Gate:
  per-dataset-run cost printed, split cached/uncached input + cache-creation + output + reasoning.
  Without this we can't prove any cost win. Re-judge not needed (measurement only).
- **Track C — un-blockable I/O (low risk, mostly built).** Adopt PR #67 search; wire `fetch-use` into
  the python sandbox + prompt the model to prefer it. Cuts the retry/give-up turns (21/26/29).
  Eval+judge; keep if score ≥ ~91 and cost/turns drop.
- **Track B — kill poll-per-call (biggest lever, highest risk).** Redesign observe/status so one
  script-run = one (few) model call(s), preserving course-correction. Eval+judge with A's cost meter;
  this is the score-vs-cost tightrope — A/B it, don't guess.

Sequence: **A → C → B.** A unblocks measurement; C is low-risk built-mostly; B is the careful one and
benefits from A's meter to prove the win.

## Open items
- Confirm `fetch-use` is installed/available in the eval python sandbox (and `BROWSER_USE_API_KEY`,
  `SESSION_ID` propagate to it).
- PR #67 needs a rebase on current main before adoption (it has several `Merge origin/main` commits).
- Track B design: decide between (a) block-until-done within a per-run deadline + stream only on a
  generous interval, vs (b) keep observe but make `browser status` free (no model call). Spike both.
