# Eval methodology — run, judge, forensically analyze, connect to code

The full loop we've been using to find real problems (not score noise), root-cause them in the
codebase, and prove fixes. Everything below is reusable machinery in /home/exedev/eval-runs/.

## 0. The mental model
- A run's ground truth is `state.db` — an `events` table (`session_id, ts_ms, type, payload_json`)
  recording every model call, tool call, tool output, and per-turn token usage. Scores are opinions;
  events are facts. Almost every insight comes from querying this table.
- `model.turn.request` = one expensive model call (verified 1:1 with `tool.started`). Everything is
  countable: turns, polls, re-scrapes, bytes returned to the model, tokens billed.
- Know the noise floor BEFORE interpreting anything: run-to-run ±4 points, judge-to-judge ±6 (killed
  by the locked judge), ~3 dataset-impossible tasks (ceiling ≈96). Score deltas <3 are weather, not
  climate. Only deterministic harness changes and architecture changes move the mean; prompt
  heuristics live below the noise floor.

## 1. Running an eval
- One experiment = one git worktree + branch, file-disjoint bisectable commits.
- `/home/exedev/eval-runs/run_wt.sh <worktree> <tag>`: builds RUN_ID, fresh state dir under
  /tmp/<run_id>, re-imports codex OAuth (tokens go stale → 401), exports env
  (BROWSER_USE_API_KEY, BROWSER_USE_EVAL_DONE_AUDIT=1, BROWSER_USE_DISABLE_LOCAL_SEARCH=1,
  LLM_BROWSER_PROVIDER_MAX_RETRIES=5), runs `dataset-run-codex real_v8 --all --model gpt-5.5
  --max-turns 10000 --concurrency 25 --browser-mode cloud`, then distills the manifest into
  `judge_packets.json` (task_id, ok, task, final_result, cwd, artifact_root, error per task).
- Do not use an 80-turn cap for `Internal_Bench_hard` parity runs. It turns long extractions into
  artificial pending/partial failures and makes the score non-comparable to raw Codex +
  browser-harness runs.
- The CLI now rejects `Internal_Bench_hard` provider runs below `--max-turns 10000`; keep that guard
  in place so direct `dataset-run-openai`/`dataset-run-codex` invocations cannot silently recreate
  the old cap.
- For `Internal_Bench_hard` browser-harness parity, use the minimal simple-harness surface rather
  than the full product browser/supervisor tool surface:
  `browser-use-terminal -c simple_harness=true -c disable_local_search=true --state-dir "$ROOT/state" dataset-run-openai /home/exedev/datasets/Internal_Bench_hard.json --all --model gpt-5.5 --max-turns 10000 --python-timeout-seconds 180 --max-attempts 1 --concurrency 25 --browser-mode cloud --run-id "$RUN_ID"`.
- Preferred current runner:
  `scripts/run-internal-bench-hard-openai.sh`. It enforces the same contract, refuses to run
  without model + cloud browser credentials, writes `run-env.txt`, runs a 30s SQLite health check,
  emits `judge_packets.json`, and prepares `$ROOT/judge/` for the locked judge. The packet
  extractor collapses duplicate session records by task id before judging, then fails if the
  packet count is not exactly the benchmark task count.
- Judge prep is reproducible with `scripts/prepare-ibh-judge.py`. It enriches runner packets from
  `state/state.db`, exports per-task native event JSONL, attaches session/artifact evidence, writes
  `packets_all.json`, splits the standard `22/22/22/22/18` packet chunks, and creates judge prompt
  and chunk brief files.
- **Health-check 30s in, ALWAYS** (one bad env var silently wastes 40 min):
  `select count(*), sum(type='browser.connected'), sum(payload_json like '%API_KEY missing%'),
  sum(type='dataset.case') from events;` → expect 25 connected, 0 keymiss.
- Background waiter on the PID → notification on completion → `cp -al` backup to /home/exedev/eval-runs.
- Run artifacts: `state/state.db` (events), `state/dataset-runs/<id>.json` (task↔session manifest),
  `dataset-run-files/<id>/task-N-attempt-1/{cwd,artifacts}` (what the agent wrote to disk),
  `judge_packets.json`, `logs/dataset-run.log`.
- `ok=false` is a RUNNER flag (no captured final result), not a quality verdict — work can exist on
  disk but be undelivered. Always check disk before calling it a fail.

## 2. Judging with subagents — the locked judge
Problem: two honest judges disagree by ±6 points, drowning real deltas. Fix: LOCK the judge.
- `/home/exedev/eval-runs/LOCKED_JUDGE.md` = one canonical kit: binary 0/1 rubric + **calibration
  anchors taken from the original benchmark's real verdicts** — specific tasks that MUST fail
  (missing source section, wrong-product substitution, emails missing at scale) and complete-
  extraction shapes that MUST pass (honest nulls ok). Anchors stop both drift directions: a judge
  that passes an anchor-fail is too lenient; one that fails an anchor-pass is too strict.
- Rules that matter: runner-fail = 0 unless a COMPLETE artifact exists on disk; spec-mandated ""
  is correct, not a placeholder; judge offline as-of run date (never re-fetch live to fail an
  answer); exact counts need evidence of completeness.
- Mechanics: **5 parallel subagents, 20 tasks each.** Each brief contains: the rubric path, the
  run's data paths, band-specific anchor reminders, and a strict JSONL output contract. Each judge
  reads the packet → inspects the actual files on disk → queries state.db for behavior → emits
  `{"task_id","score","verdict","strategy","inefficiency","reason"}`.
- The behavioral note is MANDATORY even on passes — that's where the "feel" lives. A 94 with
  20 "observe-poll churn" notes is a different product than a clean 94.
- Scores are comparable across runs ONLY because rubric+anchors are byte-identical every time.

## 3. Going deep — forensic passes over the events
Each deep-dive = one subagent with a surgical brief (paths + schema-discovery instructions + the
join key + exact questions + "tables of real numbers, not impressions" + "do NOT modify files").
The ones that found everything:

- **Wasted-step taxonomy**: classify every turn substantive vs overhead (poll churn, redundant
  re-observe, re-scrape same URL, scroll churn, failed scripts, give-up stretches, done retries);
  rank by wasted tokens. → Found 19.3% of all model calls were overhead; task 6 spent 57/80 turns
  on `browser status` polls and failed by turn exhaustion.
- **Page-content cost attribution**: per-session tool-output bytes by tool; then *replay
  attribution* — interleave `tool.output` and `token_count` by seq so a dump entering at turn t is
  charged for every later turn it sits in context. → 55% of the entire input bill is page content;
  90% of tool-output bytes come from browser_script; counterfactual offload = −42% of main-model
  input. This single number justified the sub-agent architecture.
- **Cost meter** (`/home/exedev/eval-runs/cost_meter.py`): `token_count.info.last_token_usage` per
  turn → uncached/cached/output split → $ via a labeled price table → per-task ranking + run-vs-run
  delta. (Verified: sum of per-turn `last` == final cumulative `total`, so the math is exact.)
  Key reframe it produced: 71% of $ is uncached input; the big prompt is ~99% cached → prompt
  trimming is near-worthless for $; turns and page-bytes are everything.
- **Cross-run differential forensics**: run the same queries on a good run vs a bad run and diff
  the *behavioral counters*, not the scores. 88-run vs 81-run: output truncations 0→780,
  blind-guess KeyError-class bugs 8→53 → root cause = a 4KB inline output cap added between them.

## 4. Connecting behavior to code
The repeatable pattern: **symptom string → grep → constant/branch → read the doc comment + git
history for WHY → only then change it.**
- "browser_script is still running. Next: observe…" → browser.rs:2513 → DEFAULT_OBSERVE_TIMEOUT_MS
  = 1_000 → doc comment revealed a previous 30s version REGRESSED (stacked blocks starved the task
  timebox). The fix had to respect that: raise the non-stacking start-wait + a hint, keep the floor.
- Truncation markers in outputs → MAX_INLINE_BROWSER_SCRIPT_STDOUT_BYTES (the 4KB blindfold → 120KB).
- Blocked fetches → helpers.py http_get fallback chain → fetch_use import silently failing → vendor it.
- Parity questions → run the same investigation in repos/codex (e.g. codex truncates observations to
  10k tokens with head/tail elision + a size banner; we kept 3× that with no elision).
History matters: half the "obvious fixes" were already tried and reverted for reasons recorded in
doc comments and commit messages. Read them before re-fighting old battles.

## 5. The verdict loop (after every change)
1. Build in the worktree; eval; health-check; wait.
2. Prepare judge packets if the runner did not already do it:
   `scripts/prepare-ibh-judge.py --run-root "$ROOT" --run-id "$RUN_ID"`.
3. Judge with the LOCKED judge (5 subagents). The reproducible path is:
   `scripts/judge-ibh-chunks-claude.py --judge-dir "$JUDGE_DIR" --run-root "$ROOT" --concurrency 5`.
   For a fresh Internal_Bench_hard OpenAI/cloud run, use the one-command path:
   `scripts/run-internal-bench-hard-openai.sh --run-id "$RUN_ID" --root "$ROOT" --judge`.
   After the chunks are written, aggregate them with:
   `scripts/aggregate-ibh-judgments.py "$JUDGE_DIR" --run-id "$RUN_ID" --run-root "$ROOT"`.
4. Cost-meter the run vs baseline.
5. **Diff the fail SETS task-by-task, never just the scores.** For each new fail: is it explainable
   by the change (two nav tasks fail after a nav-guidance trim → suspect the trim) or known-variance
   (tasks 33, 52)? For each fixed fail: is it the family the change targeted (6/21/26/29 blocked/
   poll-loop fails fixed by fetch-proxy + sync-script → causal, not luck)?
   For Internal_Bench_hard raw-harness parity, produce the concrete delta with:
   `scripts/compare-judged-runs.py --current-aggregate "$CURRENT_JUDGE/judge_aggregate.json" --reference-aggregate /home/exedev/eval-runs/ibh-purecodex-175254-rejudge-jsonl-20260613/judge_aggregate.json --current-label "$RUN_ID" --reference-label raw-codex-browser-harness-96 --out "$ROOT/current-vs-raw-judged-delta.md"`.
   Shortcut after judge chunks exist:
   `scripts/finalize-ibh-judged-run.sh --run-root "$ROOT" --run-id "$RUN_ID"`.
6. Keep what earns its score at acceptable cost; revert what doesn't (file-disjoint commits make
   this surgical).

## 6. Writing subagent briefs that work
- Exact paths + the join key (manifest .sessions[] maps task_id↔session.id).
- Tell them to DISCOVER schema first (`select distinct type from events`), then analyze.
- Demand numbers in tables, file:line citations with short excerpts, explicit VERDICT lines.
- Scope hard: "do NOT modify files", output format, which 5 questions to answer.
- Parallelize disjoint questions (5 judges; or capability-map + codex-parity + cost-quant +
  waste-taxonomy + prompt-audit simultaneously); keep each brief answerable in one context.
