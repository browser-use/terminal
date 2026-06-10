# Phase 1 verdict — floor fixes (eval-phase1 @ cff755d)

Locked-judge: 97/100 this run (fails 9, 72, 80) vs Phase-0 baseline 91 (fails 9,21,35,36,38,61,72,84,88).

## HONEST read: WORTH IT, but on deterministic evidence — not the score.
- DETERMINISTIC win (trustworthy, survives variance): ok=false 5 -> 1. The finalize-from-artifact
  fixes surfaced on-disk work for 35/52/61/99 (they now deliver instead of returning nothing).
  This can't regress (only surfaces work that's already complete on disk).
- The 91->97 jump is MOSTLY k=1 variance: 21/36/38/84/88 flipped to pass because this run drew
  fewer captcha walls / fuller extractions, not because of our code. Real deterministic contribution ~+2-3.
- Remaining fail 72 = genuine no-output (hit cap, no artifact) -> needs Phase 2's structural fix
  (kill the no-op control loop), not a floor patch.

## Keep. Move to Phase 2.
