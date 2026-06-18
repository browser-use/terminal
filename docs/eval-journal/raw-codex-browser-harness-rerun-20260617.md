# Raw Codex + Browser Harness Rerun - Internal_Bench_hard

Date: 2026-06-17

## Run

Raw old-style runner:

```bash
python3 /home/exedev/new-terminal/scripts/pure_codex_run.py \
  /home/exedev/datasets/Internal_Bench_hard.json \
  ibh-purecodex-rerun-full-20260617-151737 \
  25
```

Run root:

```text
/tmp/ibh-purecodex-rerun-full-20260617-151737
/home/exedev/eval-runs/ibh-purecodex-rerun-full-20260617-151737
```

Judge dir:

```text
/home/exedev/eval-runs/ibh-purecodex-rerun-full-20260617-151737-judge-tmp-root
```

The judge was prepared with JSONL session tracing enabled:

```text
packets: 106
missing Codex session files: 0
runner results: 105/106
runner no-result: 6dpbhs
```

No OpenAI quota/rate-limit failures were found in completed Codex logs. One Google task saw a site-side unusual-traffic/429 page, which is website blocking, not provider quota.

## Score

Strict judge score:

```text
90/106 = 84.9%
```

Failed ids:

```text
0kqsos, 3d5iqv, 3w5uzb, 55j2o7, 6dpbhs, afeyuh, az39pe,
e7flqh, iogxpb, jgzlma, o74v1q, q85jsg, r5l8a7, trp0j4,
y49e7e, zcotoh
```

Failure classes:

```text
missing-required-fields: 3
runner-no-result: 1
site-access-blocked: 2
source-limited: 1
source-scope-drift: 2
synthetic-or-unsupported: 1
weak-evidence: 5
wrong-record: 1
```

## Comparison

| Run | Score | Failed |
| --- | ---: | ---: |
| Old strict reference, June 13 | 96/106 = 90.6% | 10 |
| Current corrected stock harness, June 17 | 92/106 = 86.8% | 14 |
| Raw Codex + browser-harness rerun, June 17 | 90/106 = 84.9% | 16 |

## Interpretation

Simply rerunning raw Codex + browser-harness today did not reproduce the old 96/106 reference. It also did not beat the current corrected stock-harness run under the strict judge.

The raw rerun did recover some tasks that current stock missed, especially:

```text
0oxfq8, 3rzsj4, h42m44, objlzy, pvs7hz, sw2292, swebnv, up8ijl, yjb1qx
```

But it regressed or failed other tasks that current stock passed:

```text
3d5iqv, 3w5uzb, 55j2o7, 6dpbhs, afeyuh, e7flqh,
iogxpb, o74v1q, r5l8a7, trp0j4, zcotoh
```

This means the old score was not reproduced by the raw recipe alone. The remaining explanation is a mix of live-site/session variance, old Codex/global-state/version differences, and task-level stochasticity.

## Notable Task Findings

- `y49e7e`: raw rerun returned `{"response":"","sources":[]}` again after Google blocking. The old reference pass was likely live Google access/session variance.
- `q85jsg`: raw rerun produced a plausible Yad2 final, but strict judge rejected it as weak evidence because saved source artifacts showed blocking/error pages and the listing details were not sufficiently grounded outside the final answer.
- `sw2292`: raw rerun improved over current stock, saving 1560 TripAdvisor review records versus current stock's 1239 partial.
- `objlzy`: raw rerun improved over current stock by using YouTube Charts Egypt and saving `result.json`, `result.md`, chart rows, metadata, and screenshots.
- `yjb1qx`: raw rerun improved over current stock with a verified 2022 TMDB result set.
- `0kqsos`: raw rerun looked more complete than current stock but still failed strict judge because date filtering for "last 3 days" was not verifiable at scale.
- `6dpbhs`: raw runner failed with no final result.

## Conclusion

The old 96/106 is not currently reproducible by just running raw Codex + browser-harness. The raw setup still has useful behavior to mine, especially the long old prompt and aggressive alternate-source recovery, but it is not a guaranteed performance baseline today.
