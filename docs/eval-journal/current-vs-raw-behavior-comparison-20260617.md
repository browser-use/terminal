# Current Stock Harness vs Raw Codex + Browser Harness

Date: 2026-06-17

## Runs Compared

Current corrected stock harness:

```text
/home/exedev/eval-runs/ibh-stock-codex-home-skill-full-20260617-060448
score: 92/106 = 86.8%
```

Fresh raw Codex + browser-harness rerun:

```text
/tmp/ibh-purecodex-rerun-full-20260617-151737
/home/exedev/eval-runs/ibh-purecodex-rerun-full-20260617-151737
score: 90/106 = 84.9%
```

## Outcome Overlap

```text
both passed: 81
both failed: 5
current failed, raw passed: 9
raw failed, current passed: 11
```

Both failed:

```text
0kqsos, az39pe, jgzlma, q85jsg, y49e7e
```

Raw did better:

```text
0oxfq8, 3rzsj4, h42m44, objlzy, pvs7hz, sw2292, swebnv, up8ijl, yjb1qx
```

Current did better:

```text
3d5iqv, 3w5uzb, 55j2o7, 6dpbhs, afeyuh, e7flqh, iogxpb, o74v1q, r5l8a7, trp0j4, zcotoh
```

## Behavioral Summary

The two runs are close in score, but not identical in behavior.

Raw Codex + browser-harness behavior:

- More aggressive recovery after blocks or awkward pages.
- More willing to use alternate sources, APIs, cached/indexed pages, direct HTML, and ad hoc scripts.
- More likely to produce large artifacts and keep digging.
- Also more likely to over-answer from weak evidence, final-only claims, or self-authored intermediate data.

Current corrected stock harness behavior:

- More conservative when blocked.
- More likely to stop with honest null/source-limited output.
- More often has cleaner grounding for tasks it passes.
- Less likely to invent plausible-looking values when the source was blocked.
- Worse at alternate-source recovery and long extraction persistence.

## Task-Level Differences

| Task | Better Run | Difference |
| --- | --- | --- |
| `0oxfq8` | raw | Raw recovered fuller Amazon laptop product fields and audit; current had many null GPU/price/rating/detail fields after product-page blocking. |
| `3rzsj4` | raw | Raw reached Rent.com utility/monthly cost section; current could not verify listing and returned no estimate. |
| `h42m44` | raw | Raw produced schema-valid current job records; current returned only 10 LinkedIn records and drifted from the multi-source junior-entry criteria. |
| `objlzy` | raw | Raw used YouTube Charts Egypt and saved 12 videos with metadata/screenshots; current reported trending unavailable. |
| `pvs7hz` | raw | Raw produced Wikiwand timeline-panel evidence; current substituted article-text events and failed source scope. |
| `sw2292` | raw | Raw saved 1560 TripAdvisor review records; current stopped at 1239 despite visible aggregate around 1561. |
| `swebnv` | raw | Raw recovered Yelp no-website businesses via alternate extraction/audit; current stopped at DataDome/CAPTCHA with empty arrays. |
| `up8ijl` | raw | Raw followed official alternate REGA/REI indicator surfaces after target 404; current left required values mostly missing. |
| `yjb1qx` | raw | Raw verified TMDB 2022 filter and detail ratings; current returned non-2022 records. |
| `3d5iqv` | current | Current had retrieved Lowe's dimensions/material evidence; raw final had unsupported dimensions/materials and no saved artifacts. |
| `3w5uzb` | current | Current saved 50 dealer records across pages; raw only got 10 repeated/page-1 records and null website URLs. |
| `55j2o7` | current | Current grounded 44 LoopNet listings; raw hit Access Denied and produced unsupported/self-authored listing values. |
| `6dpbhs` | current | Current answered from retrieved Archives West evidence; raw runner failed with no final. |
| `afeyuh` | current | Current documented Amazon review access limits and extracted exposed reviews; raw claimed high-rated product but only partial 9/44 reviews. |
| `e7flqh` | current | Current source text supported the answer; raw answer was final-only and saved a verification page instead of the needed source. |
| `iogxpb` | current | Current MapQuest/HERE artifacts supported the incident summary; raw screenshot/evidence was centered on the wrong region. |
| `o74v1q` | current | Current grounded the Chipotle menu extraction; raw collapsed/omitted distinct items from a larger source extraction. |
| `r5l8a7` | current | Current had Viator result artifact; raw saved DataDome/403 blocker while final claimed tour values. |
| `trp0j4` | current | Current extracted top HiNative answers; raw mixed in unrelated records. |
| `zcotoh` | current | Current matched EIB pipeline export semantics; raw treated project pipeline records as tenders and drifted in scope. |
| `0kqsos` | neither | Current was blocked/partial; raw found more Indeed jobs but could not verify last-3-days posting dates at scale. |
| `az39pe` | neither | Both failed SONRIS CSV export due CAPTCHA/blocking. |
| `jgzlma` | neither | Current missed Kaufland; raw used Galaxus.ch data for Galaxus.de. |
| `q85jsg` | neither | Current stopped at Yad2 block; raw produced plausible listings but judge found them unsupported by saved source artifacts. |
| `y49e7e` | neither | Both failed Google AI Overview due Google blocking/unusual-traffic pages. |

## Conclusion

The implementation is comparable to raw Codex + browser-harness in aggregate score, but the behavior is not the same.

The useful part of raw Codex is not "the raw CLI" itself. The useful part is its aggressive recovery style: alternate sources, direct APIs, long extraction loops, and heavier artifact production.

The useful part of current is the opposite: it is less likely to pass unsupported final-only claims, and its passes are often cleaner.

The direction should be to merge the strengths:

1. Keep the controlled/current harness path.
2. Add raw-style fallback/recovery patterns.
3. Add a pre-final grounding audit so raw-style plausible-but-unsupported answers are rejected before judging.
4. Make long extraction tasks continue until count/source coverage is verified, not just until a plausible file exists.
