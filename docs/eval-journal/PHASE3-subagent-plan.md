# Phase 3 — The sub-agent architecture (B2), planned

Goal: the planner (gpt-5.5) never reads raw HTML; cheap sub-agents on the SAME browser do the
page-grunt-work and hand back distilled results. Evidence base: page content = 55% of the input bill;
offload model removes ~42% of main-model input (70–77% on scrape tasks 27/40/38/15/60/59).

Current state after `eval-everything` (94 score, $53.82/run): poll churn fixed, fetch un-blocked.
Remaining cost = $35 uncached input, still dominated by DOM dumps in the planner's context.

## What already exists (don't rebuild)
- `spawn_agent`/`wait_agent`/`send_message`/`close_agent`/`list_agents` — live-wired, codex-parity
  (`subagents/`, ~4.7k LOC). Fork modes: none / all / last-N. Result returns as distilled summary.
- Per-spawn `model` + `reasoning_effort` override; `gpt-5.4-mini` already in the bundled catalog.
  Gemini-flash = wired provider, needs a `model_catalog_json` override + GEMINI_API_KEY to be spawnable.
- Browser lease API (`claim_browser`/`with_browser_action`) — the seam for sharing.

## The one blocker
Every agent eagerly creates its OWN browser (`create_browser_for_agent` mints a fresh `BrowserId`;
child session id is distinct — entrypoint/provider.rs:468-481, runtime lib.rs:4076/5035). A child
cannot touch the planner's page/cookies/login state. This is the B2 rewire.

## Key design choice: share the BROWSER, not the TAB
A child should attach to the parent's *browser* (same Chrome, same cookies/auth/session) but open its
OWN TAB by default. This sidesteps the hard problem (two agents fighting over one page) while
delivering everything the benchmark needs:
- pagination / detail-page crawls → child tab, same session state
- parallel fan-out (N children, N tabs) → same browser context
- the rare "act on the planner's exact page" case → same-tab mode behind the existing lease lock,
  later if needed.

## Result contract: files, not context
Children write rows directly to the shared cwd (`result.json` etc.) and return ≤2KB:
`{rows_written, file, schema, anomalies}`. The bulk data NEVER transits either model's context.
This also kills the truncation dilemma: the planner doesn't need the raw dump if the child owns it.

## Phases (each = worktree + PR + eval gate, built on eval-everything)

### Phase S — offline extraction spike (FREE, ~half a day, no code)
Feed captured 120KB DOM dumps from past runs + the judged-correct outputs to `gpt-5.4-mini`
(and gemini-flash via API) with the extraction spec. Compare field-level accuracy on tasks
27/40/38/15/60/59. GATE: ≥95% agreement → the bet is validated; below → stop, rethink (maybe
structured-DOM preprocessing or a bigger mini model).
Also measures: cheap-model cost per extraction → real projected savings, not estimates.

### Phase R — repairs + search (small, 1 day, bundles into next eval)
1. Revert the browser-tool-description trim (recover nav regressions 72/98; keep the other trims).
2. Adopt PR #67: `search` tool via search.browser-use.com; remove hosted web_search. Rebase on main.
   (Fixes search-blockage the same way fetch-proxy fixed fetch.)
GATE: score ≥93, cost ≤ $54 (no regression).

### Phase B2a — shared browser core (the rewire, ~2 days)
- `spawn_agent` arg `share_browser: true` (or role default): child's browser-backend resolution looks
  up the parent/root session's browser resource and `claim_browser`s it instead of
  `create_browser_for_agent`. Child opens its own tab (new target in same browser context).
- Action-level serialization through the existing lease; child tab lifecycle on close_agent.
- Cost meter: extend cost_meter.py to aggregate child sessions' token_count into the parent run
  (children must be IN the $ number or we're lying to ourselves).
GATE: integration test — parent logs in, child reads the authed page from its own tab.

### Phase B2b — delegation ergonomics (~2 days)
- `extractor` role: gpt-5.4-mini default, fork_turns none + tight brief via input_items, browser_script
  tool only, file-output contract, hard turn cap (~15).
- Planner prompt: ONE paragraph — "for bulk page-reading/extraction/pagination, spawn an extractor;
  pass URL + spec + output file; do not read raw dumps yourself." (Generalizes; not benchmark-shaped.)
- Catalog override plumbing so gemini-flash is spawnable (optional, after gpt-5.4-mini proves out).
GATE (the big one): full eval — score ≥93 AND cost ≤ ~$40 (projection: planner uncached input
−40% ≈ −$14, children add back ~$2-4 at mini prices).

### Phase B2c — parallel fan-out (stretch, ~1-2 days)
N children, N tabs: task 12 (83 pages), 44 (249 dropdown countries), 26 (687 stores) split across
3-4 extractors. Wins wall-clock AND turns. Needs: per-child tab pinning + planner guidance on sharding.
GATE: same score, measurable wall-clock drop, cost flat-or-down.

## Task-grounded expectations
| tasks | today's pain | phase that fixes it |
|---|---|---|
| 27,40,38,15,60,59 | 70-90% of their bill is DOM replay | B2b (extractor) |
| 26,41,72 | 60-80 turn crawls, planner does every page | B2b + B2c |
| 12,44 | mechanical 83-page/249-option iteration | B2c |
| 72,98 | nav fumbling (regressed by trim) | R |
| search-blocked tasks | DuckDuckGo/native search blocked | R (PR #67) |

## Risks
- Cheap-model extraction quality — Phase S kills or confirms cheaply BEFORE the rewire.
- Planner over/under-delegation — prompt is one paragraph; eval gate catches it; tune via run traces.
- Two agents, one CDP websocket — serialize via lease; children on own tabs minimize contention.
- Child token cost — metered honestly in B2a; mini prices make even full-fork ~10× cheaper, but
  default to brief-not-fork.
- Auth: gpt-5.4-mini rides the existing codex OAuth (zero new auth). Gemini needs GEMINI_API_KEY.

## Sequence & total effort
S (½d, free) → R (1d) → B2a (2d) → B2b (2d, the payoff gate) → B2c (stretch).
~5-6 working days to the B2b gate. Cost trajectory: $66.84 → $53.82 (done) → target ~$40 at B2b,
with the SAME or better score, and an architecture that generalizes (planner+workers, not
benchmark hacks).
