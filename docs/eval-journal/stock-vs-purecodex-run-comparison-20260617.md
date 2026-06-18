# Stock Harness vs Pure Codex Reference - Run Comparison

Date: 2026-06-17

## Objective

Compare the corrected stock Codex + browser-harness run against the older pure Codex + browser-harness reference on `Internal_Bench_hard`, and explain why the old reference scored better.

## Score Snapshot

| Run | Root | Score | Failed ids |
| --- | --- | ---: | --- |
| Current corrected stock harness | `/home/exedev/eval-runs/ibh-stock-codex-home-skill-full-20260617-060448` | 92/106 = 86.8% | `0kqsos`, `0oxfq8`, `3rzsj4`, `az39pe`, `h42m44`, `jgzlma`, `objlzy`, `pvs7hz`, `q85jsg`, `sw2292`, `swebnv`, `up8ijl`, `y49e7e`, `yjb1qx` |
| Older pure Codex reference strict rejudge | `/home/exedev/eval-runs/ibh-purecodex-175254-rejudge-jsonl-20260613` | 96/106 = 90.6% | `2vxyzx`, `84xyjo`, `az39pe`, `c856wp`, `eo2t8f`, `h42m44`, `pvs7hz`, `r5l8a7`, `togn1w`, `up8ijl` |

Delta: current is net -4 tasks. This is not a simple superset regression: current failed 10 tasks the reference passed, but passed 6 tasks the reference failed.

## Runner Difference

The older pure-Codex runner was `/home/exedev/new-terminal/scripts/pure_codex_run.py`.

It ran:

```bash
codex exec --cd <task-cwd> \
  --skip-git-repo-check \
  --dangerously-bypass-approvals-and-sandbox \
  -m gpt-5.5 \
  -o <final.txt> \
  "<prompt>"
```

Important differences from the corrected stock-harness runner:

- The old runner used ambient global Codex state, not isolated per-task `CODEX_HOME`.
- The old runner used `prompts/dataset-case-user.md`, a long prompt with extraction, verification, fallback, artifact, and completion-discipline instructions.
- The current stock-harness runner used a shorter prompt in `prompts/dataset-case-simple-harness-user.md`.
- The old reference was completed from initial plus resume artifacts, not one clean one-shot run.
- The old run happened on June 12/13; the current run happened on June 17. Several failures are clearly live-site access changes.
- The old browser-harness repo/helper state may not be byte-identical to the current dirty `/home/exedev/repos/browser-harness` checkout.

## Current-Only Failures

These are tasks the old strict reference passed but the current corrected run failed.

| Task | Current failure class | Artifact-level cause |
| --- | --- | --- |
| `0kqsos` | source-limited | Old reached Indeed cards/details and saved 26 detail records. Current hit Indeed sign-in/block and only had six jobs across five cities. |
| `0oxfq8` | missing-required-fields | Old accessed Amazon product-detail pages and filled requested laptop fields. Current hit Amazon `opfcaptcha`, leaving large numbers of GPU, refresh-rate, weight, price, and rating fields null. |
| `3rzsj4` | runner-no-result | Old found a Rent.com Market St listing and screenshoted the utility estimate section. Current could not verify the listing and returned no utility estimates. |
| `jgzlma` | missing-required-fields | Old returned Amazon, Galaxus, and Kaufland top-20 supplement lists. Current got Kaufland access blocked and drifted on Galaxus. |
| `objlzy` | missing-required-fields | Old found YouTube Charts Egypt and returned trending Arabic videos. Current did not find a usable trending surface and reported it unavailable. |
| `q85jsg` | site-blocked | Old extracted Yad2 Next.js/detail data and returned listings. Current hit ShieldSquare/PerfDrive and produced no listing records. |
| `sw2292` | partial-scrape-packaged-complete | Old paginated TripAdvisor GraphQL to 1538 reviews. Current got 1239 while the visible aggregate was 1561, then packaged it as complete. |
| `swebnv` | site-blocked | Old recovered around Yelp blocking via alternate/search/cached evidence and returned 20 businesses. Current hit Yelp DataDome/403 and stopped with empty arrays. |
| `y49e7e` | site-blocked | Old reached Google AI Overview and saved source-panel screenshots. Current hit Google recaptcha/sorry pages and returned empty response/sources. |
| `yjb1qx` | wrong-record | Old validated TMDB 2022 filtered results. Current returned several non-2022 movies after a bad filter/extraction path. |

## Reference-Only Failures

These are tasks the current run passed but the old strict reference failed:

`2vxyzx`, `84xyjo`, `c856wp`, `eo2t8f`, `r5l8a7`, `togn1w`.

This matters because the old run was not globally better on every task. It won more anti-bot/dynamic-site tasks, while current won several tasks where the old reference had weak evidence, wrong scope, or source blocking.

## Shared Failures

Both runs failed:

`az39pe`, `h42m44`, `pvs7hz`, `up8ijl`.

These are not the source of the score delta.

## Why The Old One Looks Better

The old run's advantage is mostly explained by three factors:

1. Live site access was better on several blocked sites: Indeed, Amazon, Kaufland, Yad2, Yelp, Google, and Rent.com.
2. The old prompt pushed harder on long extraction, fallback, verification, and artifact repair. The current prompt is simpler, but it also made the agent more likely to stop honestly at a block or partial scrape.
3. The old runner used ambient Codex home/config/state and a resume-completed artifact set, so it was not a byte-identical reproduction target.

The evidence does not point to 429s, Rust ownership, or a runner crash as the main cause. The corrected current run completed 106/106 and the failures are mostly judged result-quality failures.

## Decisive Next Test

Add or run a literal `legacy-pure` compatibility mode:

- exact old `prompts/dataset-case-user.md`
- exact old `codex exec` command style
- shared/ambient Codex home option
- same `BU_NAME` and `BH_DOMAIN_SKILLS=0` environment shape
- same browser-harness skill checkout
- same strict judge

If that reproduces roughly 96 today, the missing behavior is prompt/home/env parity. If it scores around 92, the old 96 was mostly live-site variance and resume luck.
