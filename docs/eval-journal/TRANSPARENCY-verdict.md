# harness-transparency full-eval verdict — real-v8-transparency-20260612-174845

Branch `harness-transparency` (split actions.py/runtime, source embedded in system prompt +
seeded to cwd, agent_helpers.py auto-import, 656-tok gotchas description, skills bundle removed).
Locked 5-panel judge. Account quota healthy (0 rate-limit errors all run).

## Headline: score HELD within noise, behavior measurably BETTER, cost FLAT.

| metric | baseline (everything, 94-run) | transparency | delta |
|---|---|---|---|
| raw judge score | 94 | **88** | −6 raw |
| infra-adjusted (excl. 2 provider deaths) | 94 | ~90 | |
| + judge-strictness-adjusted (87/88/9) | 94 | **~93** | within noise |
| model turns | 2225 | **2000** | **−10.1%** |
| js() bare-arrow incidents | 123 calls / 79 empty | **0** | class extinct |
| cache hit | 71.6% | **73.5%** | best run |
| cost | $53.82 | $54.61 | +1.5% (flat) |
| agent_helpers.py written | n/a | **4 tasks** | organic uptake |
| actions.py read | n/a | 1 task | (source mostly absorbed from prompt) |

## Fail decomposition (12 raw)
- Infra (provider error, no artifact — not behavior): 1, 61.
- Calibration anchors (fail by design, both runs): 68, 74.
- Dataset-impossible, judged strict (baseline PASSED these as honest-ceiling): 87, 88.
- Strict entity-read flip (location pages vs listings; 3rd panel running to fail it): 9.
- Real/borderline: 67 (padded counts), 72 (date-serial-as-ID, the dataset's resident demon —
  3rd distinct failure mode in 3 runs), 98 (never reached the bill page), 100 (galaxus
  wrong-product third), 43 (screenshot capture silently produced 8 blank 1KB PNGs).

## vs the 94-run fail set (33,52,68,72,74,98)
- FIXED: 33, 52 (both clean passes now).
- STILL FAIL: 68, 72, 74, 98 (4 — anchors + the 2 hard cases).
- NEW: 1,61 infra · 87,88,9 strictness · 67,100 borderline · 43 the one real harness bug.

## The economics (the actual result)
The embedded actions.py source adds ~8.8M input tokens/run but removes 225 turns (−10%); net cost
flat. That is the trade we predicted. The behavior moved exactly where the thesis said: the whole
js() arrow-trap failure class is GONE, fumble-driven turns dropped, cache improved, and the model
began writing its own persistent helpers unprompted. No catastrophic regression (unlike the 48KB
truncation in prompt-diet).

## What converts flat→down: codex-shape (built, green, stacked on this branch)
Envelope kill (raw stdout, was re-emitting extracted rows twice) + 9 dead/dup tools hidden
(~1.5k tok/turn off 2000 turns). Those deletions act on EVERY turn this branch already shaved.
Eval codex-shape next (on a healthy account); if score holds, both branches PR together as the
browser-harness convergence.

## One real bug to fix (task 43): screenshot capture can silently write blank ~1KB PNGs with no
error and no retry. Pre-existing (capture_screenshot unchanged, just relocated). Add a
blank/under-size detection + one retry in capture_screenshot, and surface "screenshot looks blank"
to the model. Cheap, fixes a fail.
