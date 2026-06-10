# Phase 0 — Locked judge + trustworthy baseline (PR #105 / fix/real-v8-simple @ 84703c1)

## The locked ruler
`/home/exedev/eval-runs/LOCKED_JUDGE.md` — strict, calibrated to the ORIGINAL benchmark's real
verdicts (must fail 27/43/68/74-shape, must not over-fail 96/97/100-shape). Sits between my
lenient panel (89) and the user's over-strict panel (77). Use IDENTICALLY for every phase.

## Baseline number (run real-v8-simple-k1)
- **Locked-judge (agent capability): 90/100**  fails: 9, 21, 35, 36, 38, 61, 72, 84, 88  (87 soft-pass)
- **Product-delivered (ok=false counts as 0): ~87/100** — tasks 52 & 99 have the COMPLETE answer on
  disk (feb17_selected.json / result.json) but the runner delivered nothing. The 3-pt gap = harness
  floor leak.

## Fail taxonomy (what's actually wrong)
- DATASET-IMPOSSIBLE / dead points: 88 (40x13 verified surgeons), 87(borderline) — no agent wins these.
- LOST WORK (ok=false, fixable by harness): 35, 52, 61, 72, 99 — work attempted/saved, score discarded.
- ENVIRONMENTAL: 84 (captcha → blank screenshot).
- STABLE-HARD: 9 (entity substitution — site has no per-property pages), 21 (date-picker, 0 prices captured),
  36 (210/354 citations — captcha wall), 38 (Samlino broadband section dropped).

## Behavioral teardown — WHY it works, where it's inefficient (from per-task notes)
WHY IT WORKS (keep): API-first/source-data extraction is the win condition on every fast pass
(7, 13, 14, 25, 31, 36-ish, 78, 91) — find the JSON/endpoint, one bulk pull, done. Honest-null +
saved-file finalize also work.

DOMINANT INEFFICIENCIES (ranked by waste, these are the real cost — mostly NOT capability):
1. observe-poll / no-op churn on slow widgets & background scripts: 40 (49 turns/29 scripts for 3 rows),
   21 (18 turns toggling a date-picker, captured nothing), 22 (31 turns/22 scripts for 6 rows),
   26/29 (30-42 turns). Each poll = a wasted model turn.
2. self-inflicted script-error retry churn: 84 (51 turns fighting captcha), 88 (177 script-fails of
   monolithic thrash), 37 (12 fails on the table), 97 (9), 85 (16 retries), and a 3-9 fail tail on many.
3. monolithic long scripts that thrash/timeout instead of chunking (88, 36).
4. lost-work at finalization (35/52/61/72/99) — the floor leak.

CONCLUSION: the agent's *strategy* is largely good (API-first generalizes). The losses are
(a) ~2 dataset-impossible, (b) ~5 harness floor leaks (Phase 1), (c) churn/thrash from the
process-spawn + 2-tool + observe-poll architecture (Phase 2 target — these are turns/time, and they
tip borderline tasks into timeouts/caps). This validates the plan: Phase 1 = stop losing work;
Phase 2 = remove the churn architecture, not patch it.
