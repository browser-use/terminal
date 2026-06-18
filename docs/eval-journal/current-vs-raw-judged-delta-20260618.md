# Judged Run Comparison

## Inputs

- Current: `simple-harness-supervisor-20260618`
- Current aggregate: `/home/exedev/eval-runs/ibh-browser-harness-supervisor-goal-no80-openai-20260618-071302/judge/judge_aggregate.json`
- Reference: `raw-codex-browser-harness-96`
- Reference aggregate: `/home/exedev/eval-runs/ibh-purecodex-175254-rejudge-jsonl-20260613/judge_aggregate.json`

## Summary

| Metric | Value |
| --- | ---: |
| Current strict score | 90/106 (84.9%) |
| Reference strict score | 96/106 (90.6%) |
| Current tasks | 106 |
| Reference tasks | 106 |
| Both pass | 85 |
| Both fail | 5 |
| Current-only regressions | 11 |
| Current-only improvements | 5 |
| Missing in current | 0 |
| Missing in reference | 0 |

## Regressions Vs Reference

| Task | Current | Reference | Current class | Reference class | Current reason | Reference reason |
| --- | ---: | ---: | --- | --- | --- | --- |
| `6dpbhs` | 0 | 1 | weak-evidence | none | The saved evidence names Gifford & Prentiss and A. M. Prentiss, but it does not support the key clue that the remaining owner's wife had... | The historical-owner answer is supported by the saved XML source and session outputs. The XML states that Frank Perkins and Walter Allen... |
| `82kkzm` | 0 | 1 | source-limited | none | The Google News extraction produced five rows, but the fifth selected article is an FT security verification page with headline `Security... | The Google News extraction produced five ranked records with headline, source URL, Google News URL, and article_text fields, plus screens... |
| `8hyexf` | 0 | 1 | source-scope-drift | none | The exported GoDaddy CSV is for auction inventory but is not limited to auctions ending within 24 hours: the Auction End Time values exte... | The required exported CSV artifact exists as result.csv. A direct CSV check found 10,000 rows, the expected GoDaddy auction columns, Sale... |
| `afeyuh` | 0 | 1 | source-limited | none | The selected palazzo product and rating are supported, but the saved result includes only 9 visible product-page reviews and explicitly r... | The Amazon plazzo task produced a saved result.json with the search audit, selected highest-rated product, product page, and complete set... |
| `jgzlma` | 0 | 1 | source-scope-drift | none | The requested output was top dietary supplements across Amazon.de, Galaxus.de, and Kaufland.de, but the Galaxus section includes non-supp... | The saved result JSON contains complete top-20 lists for Amazon.de, Galaxus, and Kaufland with the requested product fields. |
| `l3gywi` | 0 | 1 | missing-required-fields | none | The runner produced a CSV, but the required VIN, miles, price, and title values were blank for the listed CarMax URLs. This is a material... | The task asked for data for 27 CarMax URLs, and the saved audit credibly shows those exact stock URLs were inactive/unavailable while a c... |
| `mly4ly` | 0 | 1 | missing-required-fields | none | The JSON contains two Arlington apartment samples with reviews and unit details, but one of the two required management_email values is a... | The Arlington Apartments.com sample contains exactly two properties with phone numbers, property details, reviews, and unit/floor-plan de... |
| `mvxpj4` | 0 | 1 | source-scope-drift | none | The saved JSON contains jobs outside the requested Tallahassee on-site, last-3-days Dice result scope, including entries whose descriptio... | The delivered job extraction is grounded in saved artifacts. result.json contains 37 Dice jobs with the requested fields, and raw_checks.... |
| `q85jsg` | 0 | 1 | missing-required-fields | none | The Yad2 output did not include the requested complete listing details. Saved artifacts show validation/blocking pages, while the final l... | The Yad2 listing extraction is supported. result.json contains 19 listings and result.txt has matching listing blocks; the rows include t... |
| `swebnv` | 0 | 1 | site-blocked | none | The Yelp task was not completed. The run reports that Yelp was blocked and no qualifying Houston restaurant/auto-repair businesses withou... | The Yelp no-website deliverable is supported despite blocked Yelp search pages. result.json contains 10 restaurants and 10 auto-repair bu... |
| `y72ivg` | 0 | 1 | source-limited | none | The artifacts support Amazon UK, JIB, and Amazon USA work, but the saved result states that eBay UK/USA exact-part searches produced no p... | The saved report contains the Amazon UK top 10 DDR5 sales-ranked items, JIB DDR5 inventory and top in-stock matches, and marketplace comp... |

## Improvements Vs Reference

| Task | Current | Reference | Current class | Reference class | Current reason | Reference reason |
| --- | ---: | ---: | --- | --- | --- | --- |
| `2vxyzx` | 1 | 0 | none | weak-evidence | The native event log contains browser-extracted search/page text for Apple, Amazon, Target, Best Buy, Walmart, B&H, and Newegg, including... | The shopping comparison contains material unsupported exact retailer rows. Some Amazon, Best Buy, and Newegg evidence was retrieved, but... |
| `84xyjo` | 1 | 0 | none | site-blocked | The native event log and screenshot confirm the exact Copilot query 'Is Browser-Use legit' was submitted and the full visible Copilot res... | The requested deliverable was Copilot's generated response text and a screenshot after submitting the exact query. The saved evidence sho... |
| `c856wp` | 1 | 0 | none | site-blocked | The estate-agent search result is source-limited but complete for the workflow: `result.json` records page 1 items, pages 2-5 outcomes, a... | The output is materially partial. The task required collecting Google results from pages 1 through 5 and a LinkedIn-snippet check, but re... |
| `r5l8a7` | 1 | 0 | none | site_blocked_unsupported | The saved Viator result artifact contains the Orlando family-friendly search results and the top three entries with the same titles, pric... | The Viator answer is unsupported. The saved artifacts are only Viator screenshots around verification/captcha or home/search attempts, an... |
| `up8ijl` | 1 | 0 | none | incomplete_extraction | The result documents the official REGA indicators URL failure/404 and the rejected dashboard portal, saves screenshots, and honestly repo... | The requested REGA indicators dashboard was not extracted. The saved result says /indicators returned 404 and the real indicators host wa... |

## Shared Failures

| Task | Current | Reference | Current class | Reference class | Current reason | Reference reason |
| --- | ---: | ---: | --- | --- | --- | --- |
| `az39pe` | 0 | 0 | site-blocked | runner-no-result | The requested October 2025 Louisiana SONLITE/ORDS Oil and Gas Production Details CSV artifact was not produced; the cwd contains only HTM... | The runner failed and no complete requested CSV download artifact exists in the task cwd or durable copy. The saved files are only header... |
| `eo2t8f` | 0 | 0 | site-blocked | source-limited | The permit task requested all permits, violations, and inspections for 1000 Biscayne Blvd, but `result.json` reports status `blocked` wit... | The deliverable admits it is incomplete and source-limited. It did not collect all Miami-Dade permit portal permits, violations, and insp... |
| `h42m44` | 0 | 0 | empty-result | source-scope-drift | The requested job listing scraper returned an empty array and `result.json` is an empty list. `checks.json` only shows attempted sources... | The job results do not follow the requested source order or source set and include unsupported date normalization. The saved output has o... |
| `pvs7hz` | 0 | 0 | site-blocked | wrong_source_scope | The required Wikiwand World War II chronological timeline was not extracted. The artifacts show a blocked/sign-in experience and saved ba... | The answer is not grounded in the requested Wikiwand chronological timeline view. The session shows navigation to Wikiwand's World War II... |
| `togn1w` | 0 | 0 | source-scope-drift | wrong_scope | The answer did not provide two current round-trip Southwest flight-deal offers with actual travel-date terms: one cited deal was a one-wa... | The task asked for current round-trip Southwest flight deals. The retrieved page evidence and final response identify one-way starting fa... |

## Full Task Matrix

| Task | Current | Reference | Current class | Reference class |
| --- | ---: | ---: | --- | --- |
| `04k43s` | 1 | 1 | none | none |
| `0kqsos` | 1 | 1 | none | none |
| `0oxfq8` | 1 | 1 | none | none |
| `0qtl3q` | 1 | 1 | none | none |
| `0vgnxb` | 1 | 1 | none | none |
| `0x65mu` | 1 | 1 | none | none |
| `1b0z3n` | 1 | 1 | none | none |
| `2vxyzx` | 1 | 0 | none | weak-evidence |
| `36zx1t` | 1 | 1 | none | none |
| `39e3nw` | 1 | 1 | none | none |
| `3d5iqv` | 1 | 1 | none | none |
| `3dd03u` | 1 | 1 | none | none |
| `3pt5r5` | 1 | 1 | none | none |
| `3qsqcq` | 1 | 1 | none | none |
| `3rzsj4` | 1 | 1 | none | none |
| `3w5uzb` | 1 | 1 | none | none |
| `4n89qk` | 1 | 1 | none | none |
| `4t60pl` | 1 | 1 | none | none |
| `55j2o7` | 1 | 1 | none | none |
| `5bwiko` | 1 | 1 | none | none |
| `5jtb9x` | 1 | 1 | none | none |
| `6dpbhs` | 0 | 1 | weak-evidence | none |
| `6gvwrd` | 1 | 1 | none | none |
| `74rs7g` | 1 | 1 | none | none |
| `7bo119` | 1 | 1 | none | none |
| `7ln23y` | 1 | 1 | none | none |
| `82kkzm` | 0 | 1 | source-limited | none |
| `84xyjo` | 1 | 0 | none | site-blocked |
| `8hyexf` | 0 | 1 | source-scope-drift | none |
| `95g3v2` | 1 | 1 | none | none |
| `983ep9` | 1 | 1 | none | none |
| `9itn7s` | 1 | 1 | none | none |
| `afeyuh` | 0 | 1 | source-limited | none |
| `ahxthv` | 1 | 1 | none | none |
| `aoim45` | 1 | 1 | none | none |
| `asnwyw` | 1 | 1 | none | none |
| `axeqbn` | 1 | 1 | none | none |
| `az39pe` | 0 | 0 | site-blocked | runner-no-result |
| `c856wp` | 1 | 0 | none | site-blocked |
| `cfzw0l` | 1 | 1 | none | none |
| `cj4t0s` | 1 | 1 | none | none |
| `dtjvzh` | 1 | 1 | none | none |
| `e7flqh` | 1 | 1 | none | none |
| `ekgysp` | 1 | 1 | none | none |
| `eo2t8f` | 0 | 0 | site-blocked | source-limited |
| `f8cgvy` | 1 | 1 | none | none |
| `gatj9t` | 1 | 1 | none | none |
| `ggsnpu` | 1 | 1 | none | none |
| `gh3015` | 1 | 1 | none | none |
| `gymdm0` | 1 | 1 | none | none |
| `h42m44` | 0 | 0 | empty-result | source-scope-drift |
| `he7ur2` | 1 | 1 | none | none |
| `hukwqv` | 1 | 1 | none | none |
| `ibljh7` | 1 | 1 | none | none |
| `ijc1yd` | 1 | 1 | none | none |
| `iogxpb` | 1 | 1 | none | none |
| `j755nf` | 1 | 1 | none | none |
| `j9a3y3` | 1 | 1 | none | none |
| `jgzlma` | 0 | 1 | source-scope-drift | none |
| `k30lvy` | 1 | 1 | none | none |
| `l3gywi` | 0 | 1 | missing-required-fields | none |
| `m5zja8` | 1 | 1 | none | none |
| `mb5m9j` | 1 | 1 | none | none |
| `mbr5xq` | 1 | 1 | none | none |
| `mlgses` | 1 | 1 | none | none |
| `mly4ly` | 0 | 1 | missing-required-fields | none |
| `mvxpj4` | 0 | 1 | source-scope-drift | none |
| `n1u349` | 1 | 1 | none | none |
| `nngh8r` | 1 | 1 | none | none |
| `o74v1q` | 1 | 1 | none | none |
| `objlzy` | 1 | 1 | none | none |
| `ozctwj` | 1 | 1 | none | none |
| `p6vz41` | 1 | 1 | none | none |
| `pvs7hz` | 0 | 0 | site-blocked | wrong_source_scope |
| `q46nou` | 1 | 1 | none | none |
| `q85jsg` | 0 | 1 | missing-required-fields | none |
| `qewaou` | 1 | 1 | none | none |
| `qtxhi1` | 1 | 1 | none | none |
| `r1dwwo` | 1 | 1 | none | none |
| `r2dhyl` | 1 | 1 | none | none |
| `r5l8a7` | 1 | 0 | none | site_blocked_unsupported |
| `rlexdw` | 1 | 1 | none | none |
| `s3kkv9` | 1 | 1 | none | none |
| `snbti2` | 1 | 1 | none | none |
| `sw2292` | 1 | 1 | none | none |
| `swebnv` | 0 | 1 | site-blocked | none |
| `t4p501` | 1 | 1 | none | none |
| `t9moge` | 1 | 1 | none | none |
| `taxp0w` | 1 | 1 | none | none |
| `togn1w` | 0 | 0 | source-scope-drift | wrong_scope |
| `trp0j4` | 1 | 1 | none | none |
| `tteavt` | 1 | 1 | none | none |
| `u0env5` | 1 | 1 | none | none |
| `ul1rsr` | 1 | 1 | none | none |
| `up8ijl` | 1 | 0 | none | incomplete_extraction |
| `v18kgy` | 1 | 1 | none | none |
| `vgww4f` | 1 | 1 | none | none |
| `xsfh4g` | 1 | 1 | none | none |
| `y0bvr6` | 1 | 1 | none | none |
| `y49e7e` | 1 | 1 | none | none |
| `y72ivg` | 0 | 1 | source-limited | none |
| `yjb1qx` | 1 | 1 | none | none |
| `yv37di` | 1 | 1 | none | none |
| `zcotoh` | 1 | 1 | none | none |
| `ze5r5u` | 1 | 1 | none | none |
| `zocfi4` | 1 | 1 | none | none |
