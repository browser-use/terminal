# prompt-diet full-eval verdict — real-v8-promptdiet-20260612-043935

Branch `prompt-diet` (cache-key 29a4f7b + skills-on-demand 170ef9e + lazy tools 45d640d +
uniform 48KB truncation a790046). Judged by the locked 5-panel judge.

## Raw result: 86/100, $56.92 — vs baseline 94/100, $53.82. NOT mergeable on this evidence.

## Fail accounting (14 fails)
- Anchors (must-fail, same as baseline): 68, 74. Hard repeat: 72 (new mode: date-serial-as-ID
  hallucination; doom-loop mode yesterday).
- Quota deaths (infra, not behavior): 30, 31, 32 — provider 429 `usage_limit_reached`. +3 adjusted.
- Judge-strictness flips (same-ish behavior, stricter panels today): 9 (location-pages borderline),
  87, 88 (dataset-impossible pair, yesterday "honest ceiling" PASS). +3 if normalized.
- REAL regressions (6): 38 (shipped empty YouSee section), 41 (timeout at 650/727), 43
  (hallucinated screenshot paths), 66 (5/11 + gave up), 67 (PADDED quotas with fallback terms —
  worse than yesterday's honest-short), 80 (gave up at 19/40).
- Real fixes (3): 33, 52, 98 (yesterday's fails now pass cleanly).

Behavior-adjusted ≈ 89–92: still below baseline. The regression family is give-up/incomplete/
fabricate on LONG COLLECTION tasks.

## Cost decomposition
- Diet token cut is real: input/turn 44.6k → 40.0k (−10%); total input −6.3% despite +98 turns.
- Cache hit 71.6% → 65.8% (−$4.6 uncached swing) — BUT all of today's runs (incl. the zero-change
  A/B arm: 62.0%) show depressed cache vs yesterday → account-saturation confound, not the diet.
- prompt_cache_key: NULL result on codex backend (A/B 62.0% vs 62.8%). Kept for parity only.
- Uniform truncation beat the mutating variant (65.8% vs 62%) — append-only confirmed right design.
- 61 outputs >48KB truncated; skill() called 0 times; spawn_agent 0 times.

## Confounds on this run (why it's not a clean verdict)
1. Account quota exhaustion mid-run (3 deaths + retry latency → the 41/80-style timeouts/give-ups).
2. Stricter judge day (3 borderline flips).
3. Possible real truncation harm on long collection tasks (41/66/80 need sight of accumulated rows?)
   — unproven; checkpoint files exist on disk for 41/97 and worked.

## Decision
HOLD the merge. Rerun the identical binary once when the account quota has cooled (≥several hours).
- If score ≥93 and cost ≤ baseline → merge (the diet is then proven cost-neutral-or-better with
  −10%/turn tokens and big headroom).
- If score stays ≤90 with the same give-up family → prime suspect is the 48KB cap on long
  collection tasks; raise TOOL_OUTPUT_TRUNCATION to Tokens(20_000) (96KB) and retest once.
- The 3 real fixes (33/52/98 patterns) and 3 judge-flips suggest the floor is ~89-92, not 86.
