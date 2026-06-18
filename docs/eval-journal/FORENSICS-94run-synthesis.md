# Trace forensics synthesis — run real-v8-everything-20260611-211259 (94/100, $53.82)

Five independent deep-trace lenses (turn accounting, failure forensics, motif mining, provable
sub-agent counterfactual, behavioral sweep). Everything below is counted from events, not assumed.
All five lens reports are in the session transcript; scratch data under /tmp/.

## The verdict that changes the plan
**The sub-agent plan, in its cost form, is largely FALSIFIED by the trace.** A full-history fork has
cache-parity with the parent on the delegated stretch — the only first-order saving is the residual
(stretch content no longer replayed in later parent turns). Measured across the top-20 sessions:
**provable saving ≈ $0.03–0.33 (<1%)**. The hoped-for ~$4 assumed forks dodge cache misses —
speculative. Mechanical stretches sit at session END (scrape→audit→done), so the residual has
nowhere to accrue.

**Where sub-agents DO provably-ish pay (the data's own lens): exploration quarantine.** Verbose
early exploration (123KB DOM/JS dumps while reverse-engineering) poisons the parent context for the
whole session: task 75 +$0.86, 66 +$0.76, 17 +$0.37, sum ≈ $2.3/run. A child that explores and
returns only "endpoint + params" kills that. HONEST CAVEAT: codex-style record-time truncation
(10k-token head/tail) captures most of the same money with no architecture. Run that comparison.
Score side: **no failed task qualifies as provably-subagent-fixable** (closest: 72, only as
compaction prophylaxis).

## The real levers, ranked (exact $ from the trace)

### P0 — deterministic harness fixes
1. **prompt_cache_key never set → $24.7–26.3/run (46–49%!).** All 100 concurrent sessions share the
   same first-256-token prefix → same cache shard → mutual eviction (cached_input flip-flops; 842
   turns cached only the static prefix; 10 sessions ~zero history caching). Plumbing exists
   (`browser-use-providers/src/lib.rs:160,1746,3826`), defaults None, nothing sets it.
   **Fix: prompt_cache_key = session id. One line.**
2. **session_done_result_text bug — task 52's fail** (`browser-use-protocol/src/lib.rs:497-509`):
   object-shaped result_file renders "<unknown>" AND discards payload["result"]. Data was on disk;
   judge saw nothing.
3. **Compaction = single point of catastrophic failure — task 72's fail.** The run's ONLY compaction
   (triggered mid-turn by one 123KB dump) produced "(no summary available)", erased all progress;
   model degenerated into 34× browser connect + 36× status (each connect provisions a FRESH cloud
   browser; no loop-breaker). Fixes: truncate a single oversized output instead of compacting whole
   history; detect empty summary → retry/keep raw tail; idempotent connect ("already connected");
   loop-breaker after 3 identical consecutive calls.
4. **Observe-poll churn v2 — $4.57.** Model polls background scripts itself; 30 polls returned
   byte-identical output; 24 stale run_ids; task 38's polls each carried 4 screenshots (+7.5k
   tok/poll). Fix: observe long-polls until output CHANGES or completion; return deltas; fix run_id
   lifecycle.
5. **js() arrow trap — 123 calls / 34 sessions / 79 silent `{}` returns.** `js('() => {...}')`
   returns the serialized function. Fix: auto-invoke 0-arg functions or hard-error.
6. **Write-echo ($1.53) + pre-connect ($0.62):** file-writes echo count+keys+sample (kills
   re-verification reads); pre-connect browser (98/100 sessions burn turn 0 on connect).
7. **Artifact-delivery primitive:** task 62 finished at t11 then spent 35 turns (74% of its cost)
   working out HOW to deliver a PDF (state.db spelunking, S3 guessing). Give done/upload a
   first-class file hand-off.

### P1 — prompt rules (small set, one eval gate)
8. **Read-back gate (the score lever, fails 33+98, near-misses 43/88/100):** before done, reconcile
   final rows against own captured evidence; "never emit Not-found for an item your own scrape
   mentions"; identifier-consistency (task-supplied ID absent from answer → flag); audits must
   spot-check 3 rows semantically, not just required_present (audit theater: 71 calls/51 sessions,
   structural only).
9. **URL provenance (fail 74):** tracker-pattern links (gotostore/redirect/transition) must be
   resolved to the real store domain before emission.
10. **Batch-probes ($7.51 motif):** 354 wasted single-probe turns (median extract output 229B vs 37k
    context resend); "batch independent probes into one script"; consider a page-recon primitive.
11. **Wrap-up discipline ($1.85+$1.76+clipboard):** verify result file at most once; done from file
    (never retype — task 30 typed the same JSON 3×; task 26's done = 27k output tokens); loop
    serial fetches inside one script.
12. **No silent proxies (H9):** unverifiable field → null + note (100: supplier=first word of title;
    99: discounted_price=price).
13. **web_search: 0 uses in the entire run** while 17 sessions hand-scraped SERPs and consumed junk.
    Fix description/nudge, and make blocked fetches (status None / Cloudflare page) LOUD — 68 lost
    findable emails to silently-parsed block pages.

### P2 — architecture (only where data says)
14. Record-time truncation of giant tool outputs (codex 10k head/tail) — kills context bombs
    (75:+105k tok, 66:+60k, 38:+35k) AND the 72 compaction trigger. Compare vs:
15. Exploration-quarantine sub-agent (share_browser child explores, returns distilled facts ≤2KB) —
    the one data-backed sub-agent role. share_browser is already implemented (subagents branch).

## Fail root-causes in one line each
33: equality-only volume match + "Not found" with the answer in context + structural audit blessed it.
52: model answered with a pointer; harness bug erased even the pointer (data was complete on disk).
68 (anchor): emails mostly not on-site; Cloudflare block pages silently read as "no emails"; web_search unused. Baseline-identical.
72: 123KB dump → mid-turn compaction → empty summary erased progress → connect/status doom-loop (34+36 calls, fresh browser per connect) → step-limit death.
74 (anchor): gotostore redirect hrefs emitted as store links; zero redirect resolution; audit never checked domains. Baseline-identical.
98: portal rejected the parcel; first fallback row (wrong account type, ID mismatch visible in its own done payload) shipped unverified.

Note: the prompt-trim suspicion for 72/98 is now DISPROVEN — both have deeper, named causes.

## Top-7 turn accounting (lens 1)
44% of turns / 57% of paid input tokens sit in ≥3-turn decision-free stretches; three cost
mechanisms: context bombs replayed every turn; poll/timeout churn; paying for the answer 3× at the
end (audit echo + cat + inline done).
