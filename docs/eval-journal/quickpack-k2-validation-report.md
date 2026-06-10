# Quick-pack validation: k=2 runs, strict-judged (2026-06-10)

Code: `fix/real-v8-restore-88-v2` @ e04ff76 (rungs 0–4 + quick-wins + efficiency pack).
Runs: `real-v8-qwpack-k1-…051745`, `real-v8-qwpack-k2-…055927` (conc 25, max_attempts 1, audit on).
Judge: strict calibrated panel (reproduces 88-baseline=89, 81-baseline=80). Matrices: `/home/exedev/eval-runs/k1k2_matrix.csv`.

## The number

| Run | Strict score |
|---|---|
| 88-baseline (old) | 89 |
| 81-baseline (regressed) | 80 |
| 85-run (rungs 0–4 only) | 85 |
| **K1** | **82** |
| **K2** | **90** ← first run to beat the old baseline |
| **k=2 mean** | **86** |

The k1↔k2 spread (8 points on IDENTICAL code) is the loudest finding: single-run scores carry ±4 of pure stochastic path/site/judge luck. The mean moved 85→86; the distribution's ceiling moved 85→90.

## Head-to-head vs the 88-run (same panel)

**K2 vs 88: K2 wins net +1.** 88 beats K2 only on 24, 33, 59 — all three are *hallucination-luck* tasks (24: K2's final JSON contradicted its own saved result.json; 33: K2 invented the requested 10/30GB tiers — the exact anchor-8 sin — while K1 honestly reported they don't exist and PASSED; 59: K2 invented an email). K2 beats 88 on 36, 43, 75, 91.
**K1 vs 88:** K1 loses on 11 stochastic recurrences of known classes (see below).

**Stable both-fail core (7):** 9 (entity-frame substitution), 15 (brand coverage at scale), 27 (extraction quality), 74 (product matching gates), 67/87/88 (impossible quantity specs — dataset issues, ~3 permanently dead points).

## Did the quick-pack mechanisms work? (telemetry + verdicts)

| Fix | Evidence | Verdict |
|---|---|---|
| Audit `""` ≠ placeholder | task 94 PASSES in BOTH runs (was audit-coerced fail); audit rejections 8→1/run | ✅ confirmed |
| Wording-police removal | k2-72 PASSED with an honest negative + documented negative control + screenshots — judges cited it as anchor-5 evidence; 75 passes honestly in both | ✅ confirmed |
| Anti-laundering prompts | 75 no longer domain-copies practice_name (both runs); 72 fixed in k2; k1-72 still hallucinated provenance → probabilistic, not deterministic | ◐ works, stochastic |
| Reality-probe audits | 26: both runs reconciled against the API total (687=687); 32-k2: 102 stores incl. all Chicago; 32-k1 still 70 | ◐ works, stochastic |
| Compaction fix | 6 compactions in k1 with ZERO task-24-style meltdowns | ✅ no recurrence |
| done-nudge | 0 firings — no leaked-monologue endings occurred | ✅/untested |
| Observe sentinel | "stop polling" note fired 106/76×; observes 287→270/212 (k2 −26%) | ✅ modest |
| Efficiency overall | minutes 629→669/615, turns 2101→2222/1970 | ≈flat (big items — events track, warm runtime — not in yet, by design) |

## What is still broken (ranked, the next targets)

1. **Hallucination-at-finalization is now the #1 real pathology** (k1-11 fabricated file contents; k2-24 final JSON contradicting its own result.json; k2-33 invented tiers; k2-59 invented email; k1-43 blank screenshots shipped). Mechanism: when the model RE-TYPES or recalls instead of reading from its saved artifact, it fabricates. Fix: make `done(result_file)` the enforced default when a result file exists (harness assembles the final from the file; inline `result` only when no file exists) — this converts a behavioral rule into a mechanical guarantee.
2. **Audit-rejection → turn-cap → nothing captured** (k1-22, k2-15): one rejection late in a run leaves no budget to repair; the artifact fallback found no `result.*` file because checkpoints used other names. Fixes: (a) fallback should also match the newest *.json/*.csv checkpoint, (b) audit rejections within N turns of the cap should accept-with-warning instead.
3. **Frame/entity substitution persists stochastically** (9 both runs, 40-k1, 60-k1's invented 24/page pagination): the page-frame rule needs to be harder ("mirror the site's own pagination size; verify the first row of your output appears on the rendered first page").
4. **Coverage-at-scale tasks** (15: brand requires walking ~29 brand categories; 27: per-card provider quality; 61-k1: 5 of 28 products) — chunked-checkpoint loops exist now but the model still sometimes stops at the first pass; needs the reality-probe habit to bind on *category* coverage, not just totals.
5. **Variance itself**: 11 tasks flipped between identical runs. Until k≥2 is the standard protocol, single-run claims are noise. The scripted pipeline (`/home/exedev/eval-runs/run_real_v8.sh`) makes k=2 one command.

## High-level heuristics (cumulative, validated across 4 strict-judged runs)

- Feedback visibility > everything (the 4KB cap was worth ~5 points alone).
- Honesty must be mechanically cheaper than confidence: every gate that punished honest wording produced laundering or fabrication; every fix that made honesty acceptable (null/"" rules, wording-police removal, negative controls) flipped tasks to PASS.
- Audits must reconcile against external reality, not self-consistency.
- Final answers must be file-derived, never memory-retyped (the remaining hallucination class).
- The efficiency frontier is the events track (observe churn) + warm runtime (script startup), not more prompt rules.
- ~3 points of this dataset are unwinnable (67/87/88 impossible specs); the practical ceiling is ~96–97.
