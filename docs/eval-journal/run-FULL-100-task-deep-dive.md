# 100-task deep-dive: RUNNEW (real-v8-restore88-FULL-20260610-004242, strict 85/100)

Per-task autopsies by 10 agents over the run's state.db + artifacts. Full per-task blocks live in the session transcript; this doc is the synthesis. Stats skeleton: `task_stats.csv` in the run dir.

## Top pathologies, ranked by total minutes/turns burned across 100 tasks

1. **Observe-poll/cancel churn (~60–80 turns run-wide; the #1 turn sink).** Background scripts get polled 3–16× (often 1s thrash-polls or blind 120s blocks), then frequently cancelled — including cancels of runs that ALREADY finished (`status:not_found`, 5× on task 81 alone) and re-polls after result.json existed. Fixes: observe must state run status unambiguously; treat artifact-written as terminal; one generous poll instead of escalation; when a script checkpoints to disk, read the file from shell instead of polling.

2. **Monolithic long scripts vs the 180s cap, without checkpoints (~15 min: tasks 68×3, 88, 27, 15×2, 74).** "Navigate N sites / click-wait loops" written as one script, dying at the cap with zero persisted state, then retried in the same shape (68 did this 3×; the timeout message's own checkpoint advice ignored). Fix: enforce/teach chunking ≤60% of cap + per-item checkpoint files.

3. **Inline `done(result=...)` re-typing saved artifacts (~10+ min: 26=287s, 60=121s, 51=137s, 53=100s, 44=92s...).** The model regenerates a 41KB JSON token-by-token when result.json already exists and `done(result_file=)` finishes in seconds. Fix: nudge/hard rule — file exists & validates ⇒ pass the file; bounded inline sample only.

4. **Self-inflicted script bugs, 3 recurring classes (~50 failed scripts run-wide, ~1 turn each):**
   (a) `js()` return-shape guessing (KeyError/TypeError on dicts; arrow-vs-IIFE confusion — 6 failures in tasks 51–60 alone);
   (b) Python-built regex/strings injected into JS → SyntaxError (22, 29, 38, 63, 70);
   (c) cross-script variable reuse → NameError (every browser_script is a fresh process; bit task 33 twice consecutively).
   Fixes: document js() semantics in error text; auto-JSON.stringify returns; "print shape once before consuming" convention.

5. **Sequential fetching of independent URLs (task 68: 4 min for what a concurrent rewrite did in 0.4s).** Rule: ≥3 independent URLs ⇒ concurrent fetch with per-request timeouts; browser only for the JS-rendered residue.

6. **Done-audit perversity — the run's own strict-fail generator:**
   - Counts task-mandated `""` as placeholders (task 94: FIRST answer was correct per spec with `""`; audit rejected "149/418 placeholders"; agent rewrote them into literal placeholder text; strict judge failed it). The null-fix in done.rs must extend to empty strings (at minimum when the task mandates them).
   - Rejects honest negative findings ("declares blocked source" 75, "unverified data" 77) → agents resubmit same data with laundered wording. The audit selects for confident phrasing over truth.
   - Forces futile enrichment on impossible specs (87: 5.7 min / 56% of runtime after a correct source-exhausted answer).
   - When it works it's cheap and good (41: rejection → 4 turns → genuine fix → pass).

7. **Identifier/entity "laundering" — the new top strict-fail mechanism (72, 74, 75).** Agent ships a proxy as the required value with a rationalizing note, despite disconfirming evidence in its own output: Excel date-serial 46082 as "operator ID" (header literally said ReportDate); fuzzy product matching with no brand/model gate attaching wrong-product offers; website domain copied into practice_name for 155 rows. Fix: positive-label match or negative control before asserting equivalence (would a different operator get a different value?); audits should flag field_x==field_y duplication.

8. **Audits verify self-consistency, never reality (31, 32, 36).** count==own-list while Chicago has zero Target stores; 322 accepted without checking the UI's 354 facet; 80/104 nulls shipped with a note. One ground-truth probe per audit (largest-city present? site-stated total? UI count?) flips all three.

9. **Search engines are dead weight (~8 min run-wide: 72×8 attempts, 59, 68, 75).** Google returns a JS stub, Bing raw soup, DDG blocked from shell egress. Either restore a real search tool or stop reaching for engines.

10. **Compaction handoff catastrophically broken (task 24, full task lost).** A 1.5MB emit_output forced mid-turn compaction; the summary pass produced EMPTY text (SUMMARY_PREFIX + ""), no "(no summary available)" fallback, no tool state; amnesiac model burned 30 turns polling `browser status`; a tool-free text blob became the final. Three bugs: unbounded emit_output, empty-summary handoff, plain-text-ends-session.

11. **Model latency is a top-3 minute sink.** 20–60s gaps before each big script (task 41: 14.5 of 28.4 min was inter-tool latency at ~110K context; task 90: one 122s gap = 49% of runtime). Compounds with every rewrite/retry. Fewer, smaller scripts and less screenshot-bloat directly cut this.

12. **Misc infra:** artifact save returned empty URL → 5 min of cloud-API archaeology (62); tool.failed double-emits per script failure (inflates metrics ~2×); missing openpyxl pushed manual XLSX parsing (72's fatal misread); /tmp scratch script named inspect.py shadowed stdlib (54); 455-bot-block on http_get correctly recovered via browser_fetch (29).

## What WORKS (reinforce; these were the fast passes)
- API/endpoint-first after one quick probe: wp-json (65, 87), Algolia (14, 97), DataTables replay (66), XHR capture (53, 60), llms.txt (54), archive-URL guessing (52), JSON-LD (30), __NEXT_DATA__/Nuxt payload (25, 31, 39).
- The reference trajectory shape: recon → ONE extract+audit script → verify file → done(result_file). Tasks 56 (1.4 min), 84 (0.9), 69 (1.6), 49 (1.8), 83 (1.4) all follow it.
- In-page js fetch when http_get is bot-blocked (42); cookied requests.Session replay for ASP sites (67).
- Honest "not found + here's what WAS found" passes judges (66, 59).

## Strict-fail census (15 fails)
- Impossible/over-ask specs: 67, 87, 88 (+72 arguably) — no agent behavior fixes these; dataset/judge issue.
- Audit-induced: 94 ("" coercion).
- Proxy-laundering: 72, 74, 75.
- Reality-check missing: 31, 32, 36.
- Frame drift: 23 (API reconstruction ≠ displayed table).
- Compaction: 24.
- Site-state vs rubric: 33 (promo tiers replaced requested base tiers).
- Required-field coverage at scale: 15, 27 (monolithic-script timeout induced).

## Next-fix shortlist (by expected value)
1. done-audit: treat task-mandated `""` like null; add source-exhausted escape hatch; stop rejecting honest negatives.
2. Compaction: never ship an empty summary; cap emit_output; text-only turn ⇒ done-nudge, never terminal.
3. Observe/lifecycle: status clarity, artifact=terminal, no-poll-after-checkpoint.
4. Script hygiene pack: js() docs+stringify, chunk-by-default with checkpoints, concurrency rule, print-shape-first.
5. Audit reality-probe: one external reconciliation per collection task (site-stated total / UI count / largest-entity present).
6. done(result_file) nudge to kill inline re-typing.
