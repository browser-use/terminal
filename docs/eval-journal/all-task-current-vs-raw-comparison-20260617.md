# All-Task Current vs Raw Codex Browser Harness Comparison

Date: 2026-06-17

## Runs Compared

Current corrected stock harness:

```text
/home/exedev/eval-runs/ibh-stock-codex-home-skill-full-20260617-060448
Score: 92/106 = 86.8%
```

Fresh raw Codex + browser-harness rerun:

```text
/tmp/ibh-purecodex-rerun-full-20260617-151737
/home/exedev/eval-runs/ibh-purecodex-rerun-full-20260617-151737
Score: 90/106 = 84.9%
```

## Top Line

The aggregate scores are close, but behavior is not identical.

```text
both passed: 81
both failed: 5
current failed, raw passed: 9
raw failed, current passed: 11
```

Current is more conservative and usually better grounded. Raw Codex is more aggressive and sometimes more complete, but it also more often produces plausible answers that are weakly supported by saved artifacts.

## Chunk 001-022

| Task ID | Outcome current/raw | Behavior | Current behavior | Raw behavior | Why result differed or why same |
|---|---:|---|---|---|---|
| `cfzw0l` | pass/pass | different | AliExpress smartwatch, 447 reviews, 60 recent reviews. | Different smartwatch, 1397 reviews, 20 latest reviews. | Both satisfied more than 20 reviews and grounded the review summary. |
| `y72ivg` | pass/pass | different | DDR5 report with Amazon UK, JIB, eBay, Amazon US blocked/unavailable. | DDR5 report with Amazon UK, JIB, Amazon US, eBay UK/US matches. | Both met core comparison; raw had broader Amazon US coverage. |
| `3qsqcq` | pass/pass | same | 58 Amazon UK smart-lighting products with seller fields. | 48 products with ASIN/detail-page seller evidence. | Same extraction target; count/evidence depth varied but both complete enough. |
| `0oxfq8` | fail/pass | different | 32 laptop rows, many required product fields null due Amazon blocks. | 44 rows across two pages, fuller required fields and audit. | Current failed missing required fields; raw recovered enough detail. |
| `afeyuh` | pass/fail | different | Selected 4.7-rated palazzo, extracted 8 public written reviews, documented blocks. | Selected 4.9-rated palazzo with 44 reviews, extracted 9 visible reviews. | Current got credit for exposed-review limitation; raw failed because complete review list was incomplete. |
| `dtjvzh` | pass/pass | same | 76 Alpha-GPC placements; `data-dib-asin` absent. | 76 Alpha-GPC placements; same limitation. | Same behavior and same site limitation. |
| `ekgysp` | pass/pass | same | 15 Amazon Puma shoe rows with image/title/description/price. | 15 rows with same required fields and audit. | Same deliverable. |
| `f8cgvy` | pass/pass | different | 37 rendered Apartments.com Burleson placards. | 151 records, including nearby results with location flags. | Both passed; raw extracted broader coverage. |
| `mly4ly` | pass/pass | same | Two Arlington apartment samples with contacts, reviews, units; emails unavailable. | Same two-sample structure. | Same evidence pattern. |
| `8hyexf` | pass/pass | same | GoDaddy filtered CSV export, 10,000 rows. | Same export with screenshots. | Same export workflow and result. |
| `3w5uzb` | pass/fail | different | 50 unique dealer records across pages 1-5 via app hash pagination. | 10 unique records; direct `?pg=N` repeated page 1. | Current found a working pagination route; raw self-reported partial coverage. |
| `0qtl3q` | pass/pass | same | 194 Volkswagen ID.4 listings under GBP 20,000. | 186 matching listings. | Same filtered Autotrader extraction; count drift acceptable. |
| `95g3v2` | pass/pass | same | Wayback Banana Republic 2025 vs 2024 promo comparison. | Same comparison. | Same research behavior. |
| `y0bvr6` | pass/pass | same | Answered: "So we had to let it die." | Same answer. | Identical clue trail. |
| `04k43s` | pass/pass | different | 119 Square jobs; dates/newly-added mostly null. | 119 Square jobs; posted dates where available. | Raw captured more date metadata; both accepted. |
| `j9a3y3` | pass/pass | different | Bloomberg Opinion returned SpaceX/Fed/Irish Border titles. | Returned Walmart/Democrats/Cracker Barrel titles. | Page content differed; both DOM-supported. |
| `4t60pl` | pass/pass | same | 30 Danish broadband packages. | 26 broadband packages. | Same two-page package extraction; count drift accepted. |
| `ul1rsr` | pass/pass | same | BuzzFeed Bottle Cap Challenge headline. | Same headline. | Identical target and answer. |
| `rlexdw` | pass/pass | different | CAFC page 504, used official RSS, 50 rows. | Scraped full CAFC table/API, 18,656 rows. | Both passed; raw had much deeper source coverage. |
| `5jtb9x` | pass/pass | same | Cafepharma active threads with topics, sentiment, quotes. | Same deliverable with fuller crawl evidence. | Same behavior; raw evidence more exhaustive. |
| `983ep9` | pass/pass | same | Campspot JSON for L'Acadie site types and availability. | Same park/date JSON. | Same API/UI-backed result. |
| `k30lvy` | pass/pass | same | 24 Carbon Pulse articles for Feb. 17, 2026. | Same 24-article markdown list. | Same complete article set. |

## Chunk 023-044

| Task ID | Outcome current/raw | Behavior | Current behavior | Raw behavior | Why result differed or why same |
|---|---:|---|---|---|---|
| `l3gywi` | pass/pass | same | CSV for 27 CarMax URLs; unavailable fields explained by 404/no data. | Same 27-row CSV with 404/API evidence. | Both honestly represented unavailable listings. |
| `mlgses` | pass/pass | same | Solved first Chess.com trending puzzle. | Solved puzzle; success message shown. | Both reached success state. |
| `o74v1q` | pass/fail | different | Broad Chipotle menu extraction with categorized items across menu/catering/high-protein. | Smaller final-only table; no saved artifacts. | Raw omitted distinct menu items/details; current grounded enough. |
| `ggsnpu` | pass/pass | same | CNN homepage/section leads with headlines, links, summaries. | Same style extraction. | Both grounded in live DOM/session evidence. |
| `gatj9t` | pass/pass | same | CompareFirst filters applied; 74 quote rows. | Same scenario and rows. | Same requested insurance setup. |
| `84xyjo` | pass/pass | same | Captured Copilot response with screenshots/session evidence. | Same. | Both submitted exact query and preserved response. |
| `gymdm0` | pass/pass | same | 500 contiguous hourly RSRUSDT candles from Binance source files. | Same. | Both verified row count, window, continuity. |
| `tteavt` | pass/pass | same | 455 deportation-tracker map marker rows. | Same 455 marker extraction. | Same marker-backed data. |
| `mvxpj4` | pass/pass | different | Dice filters applied; 27 job cards from first two pages. | Dice filters applied; 32 site-reported jobs. | Same workflow; live count/pagination differed. |
| `ahxthv` | pass/pass | same | 518 GitHub Copilot docs pages structured. | Same 518-page extraction. | Same discovered documentation set. |
| `e7flqh` | pass/fail | different | Answered Rudolf Kleinpaul with supporting OAPEN extracted text. | Final only said `Kleinpaul`; artifacts lacked support. | Raw looked right but failed weak evidence. |
| `r2dhyl` | pass/pass | same | FAA DRS Boeing 777-300 AD extraction, 131 rows. | Same 131-row AD extraction. | Same filters/dates/fields. |
| `9itn7s` | pass/pass | same | Two Henrico court records plus negative searches. | Same two matching records. | Same relevant cases. |
| `zcotoh` | pass/fail | different, close | Official Excel-backed EIB pipeline export, 1061 records. | Browser/API EIB pipeline records, 1062 records, tender fields mostly `N/A`. | Current accepted as official export; raw failed tender-vs-project scope drift. |
| `yv37di` | pass/pass | different | 40 Elgiganten SSDs; 39 exact PriceRunner matches, one miss. | 39 Elgiganten SSD cards with PriceRunner offers. | Same comparison; count variance tolerated. |
| `5bwiko` | pass/pass | same | FERC CP20-21 status/construction PDF extracted. | Same. | Same qualifying report. |
| `39e3nw` | pass/pass | different | 8 EMA power-system proposal records plus first PDF. | 9 proposal records plus first PDF. | Same relevant proposal/PDF flow. |
| `n1u349` | pass/pass | different | FedEx Ground base rate: `$24.54`; optional fees noted. | Total rate `$30.92`, including fuel surcharge. | Both grounded; raw gave fuller total. |
| `0vgnxb` | pass/pass | different | Yorvipath FAERS extraction: 243 reaction terms. | 245 rows with dashboard totals. | Same product/fields; small row-count variance. |
| `u0env5` | pass/pass | same | VYKAT XR latest date 2026-06-15; 15 cases, 43 AE terms. | Same window/counts. | Same substantive summary. |
| `cj4t0s` | pass/pass | same | FPDS Atom feed sorted by Date Signed; 30 rows. | Same 30-row result. | Same FPDS scope. |
| `hukwqv` | pass/pass | different | German telecom JSON; some Freenet Flex tiers left null as unverifiable. | 29 package objects; Freenet Flex 7/12/20 GB priced. | Both covered fields; current more conservative. |

## Chunk 045-066

| Task ID | Outcome current/raw | Behavior | Current behavior | Raw behavior | Why result differed or why same |
|---|---:|---|---|---|---|
| `ijc1yd` | pass/pass | same | Detailed GIC design audit with screenshots/extracts. | Same. | Both produced supported site-wide visual/developer audit. |
| `h42m44` | fail/pass | different | 10 LinkedIn-only jobs; judge found non-junior/mid roles and weak source diversity. | 7 LinkedIn records with exact 2026-04-10 dates; other portals attempted. | Raw judged sufficient; current failed level/source requirements. |
| `mbr5xq` | pass/pass | same | 200 newest Google reviews with name/rating/text/date. | Same. | Same newest-review extraction. |
| `t4p501` | pass/pass | same | 40 Google Maps leads across requested searches. | Same. | Same category/city/rating/review constraints. |
| `y49e7e` | fail/fail | same | Empty AI Overview response after Google CAPTCHA/sorry pages. | Same. | Neither established no AI Overview from accessible SERP. |
| `c856wp` | pass/pass | different | Reported Google block and no extracted estate-agent findings. | Compiled 35 contacts from snippets/direct pages despite Google CAPTCHA limits. | Raw did more extraction; current got credit for honest source-limited reporting. |
| `j755nf` | pass/pass | same | 6 Pakistan AI Engineer jobs, 3 LinkedIn and 3 Glassdoor. | Same. | Same exact count/source split. |
| `trp0j4` | pass/fail | different/weird | Judge accepted session-backed HiNative top-three extraction, though final text looked questionable. | Final ranked unrelated breakfast/comment records as answers 2-3. | Raw failed wrong-record; current passed on session evidence. |
| `3dd03u` | pass/pass | same | Imgflip frog/Kermit meme with only "Enjoy your life." | Same. | Same artifact-supported meme. |
| `0kqsos` | fail/fail | different | Partial Indeed scrape, only 6 jobs across 5 cities. | More rows, but many dates "Not specified" and city buckets incomplete. | Both failed coverage/date verification. |
| `74rs7g` | pass/pass | same | Google Instagram profile plus official Google company details. | Same. | Same supporting pages. |
| `ibljh7` | pass/pass | same | DNA and Telia Helsinki home internet offers. | Same. | Same package/price/speed evidence. |
| `v18kgy` | pass/pass | same | Large Kickstarter/Gamefound workbook/JSON plus script. | Same. | Same project extraction. |
| `ozctwj` | pass/pass | same | KPN/Simyo/Youfone mobile package rows. | Same. | Same provider-page coverage. |
| `55j2o7` | pass/fail | different | 44 LoopNet listings backed by page body/schema artifacts. | 44 records, but live artifacts were Access Denied and values looked self-authored. | Current source-backed; raw unsupported/synthetic. |
| `3d5iqv` | pass/fail | different | Lowe's first three porch swings supported by session payload. | Similar final table, but dimensions/materials lacked saved/session support. | Raw failed weak evidence. |
| `iogxpb` | pass/fail | different | HERE/MapQuest artifacts supported I-95 traffic and construction incident. | Claimed disabled vehicle/no other incidents, unsupported and screenshot off-area. | Raw failed weak evidence/wrong incident. |
| `0x65mu` | pass/pass | same | McGrath selector JSON validated by DOM/screenshots. | Same. | Same selector workflow. |
| `eo2t8f` | pass/pass | same | Miami-Dade property, permits, no violations/inspections with portal caveats. | Same. | Same null findings tied to checked sources. |
| `3pt5r5` | pass/pass | same | Lindt availability tables/counts across Zurich/Geneva/Basel. | Same. | Same API/session totals. |
| `aoim45` | pass/pass | same | 35 German mobile/broadband plan records. | Same. | Same complete fields/provider support. |
| `82kkzm` | pass/pass | same | Five Google News Modi articles with text and URLs. | Same. | Same top-five article extraction. |

## Chunk 067-088

| Task ID | Outcome current/raw | Behavior | Current behavior | Raw behavior | Why result differed or why same |
|---|---:|---|---|---|---|
| `asnwyw` | pass/pass | same | Found Ohio UCC filing `OH00273751505` with debtor, secured party, collateral, PDFs. | Same Ohio UCC filing and core fields. | Same official UCC record. |
| `snbti2` | pass/pass | same | OpenRouter paid model table, 307 nonzero paid rows. | 311 rows including variable-price rows, plus CSV/JSON/audit. | Same target; minor inclusion/count difference accepted. |
| `he7ur2` | pass/pass | different | USPTO search: 3 publication numbers as zero result; 2 grants had claims. | Extracted claims for all 5 via Google Patents after USPTO navigation. | Both passed; raw used alternate source and got more claim text. |
| `q46nou` | pass/pass | different | Quora Blog space; Adam D'Angelo, 545,942 followers. | Blogging Help space; Abhishek Padhi, 199 followers. | Task allowed any recommended space/top contributor. |
| `up8ijl` | fail/pass | different | REGA URL 404/security-blocked; values mostly "Not displayed." | Followed REGA/REI sources and saved numeric KPI tables, payloads, reports, screenshots. | Raw recovered official alternate indicator sources. |
| `3rzsj4` | fail/pass | different | Could not verify Rent.com listing; no utility estimates. | Found listing and expanded costs; electricity `$78-$98/mo`. | Current never reached deliverable; raw did. |
| `mb5m9j` | pass/pass | same | Samlino mobile 78, broadband 11, YouSee 3, Norlys 2. | Same counts and structured set, with more screenshots. | Same multi-site extraction. |
| `7bo119` | pass/pass | different | Science.org medicine articles dated 10 Jun 2026. | Health and Medicine category articles dated 12 Jun and 29 May 2026. | Different article choices, both supported. |
| `zocfi4` | pass/pass | different | Browser ScienceDirect filters showed 132 Harvard/open-access/2022-2026 results. | CAPTCHA-gated; used OpenAlex/Elsevier metadata and got 41. | Different source path/count; both accepted. |
| `nngh8r` | pass/pass | same | Extracted 10 SEC S-1/A financial statement tables. | Same 10 statement families. | Same SEC filing extraction. |
| `36zx1t` | pass/pass | same | Detailed Shopify Winter 2026 Editions markdown summary. | Same. | Same deliverable. |
| `az39pe` | fail/fail | same | Blocked by SONRIS CAPTCHA/terms; no October 2025 CSV export. | Blocked by reCAPTCHA; no CSV rows/artifact. | Same site-access failure. |
| `togn1w` | pass/pass | same | Southwest sale, two Knoxville route deals, round-trip link dates Jul 9-12 2026. | Two Detroit route deals, same dates. | Task allowed any two current deals. |
| `7ln23y` | pass/pass | same | Taste of Home low-carb filter; three recipes with cook times. | Same workflow, different recipes. | Task allowed any three filtered recipes. |
| `4n89qk` | pass/pass | same | TenderNed extraction: 162 postings, 113 contract values found. | 165 postings, 122 values found. | Same API/list-detail behavior. |
| `p6vz41` | pass/pass | different | Ethnic World result: 17 products over INR 1000 with full fields. | 50 products over INR 1000 with full fields. | Raw more exhaustive; both field-complete. |
| `vgww4f` | pass/pass | different | The Hindu Health COVID result: 2020 vaccine delivery article. | 2023 sudden deaths article. | Different first article surfaced; both supported. |
| `yjb1qx` | fail/pass | different | TMDB output included non-2022 movies. | Correct 2022 filtered first five. | Current filter failed; raw applied primary release date filter. |
| `6gvwrd` | pass/pass | same | Thiscorner/Square catalog scrape: 237 active products. | Same 237 products with audit. | Same catalog-source behavior. |
| `axeqbn` | pass/pass | same | Combined plastic surgeon sources; 441 candidates. | Combined 233 surgeons, all 13 specialty buckets covered. | Same strategy; current larger candidate pool. |
| `sw2292` | fail/pass | different | TripAdvisor JSON had 1,239 reviews, visible total around 1,561. | JSON had 1,560 unique reviews. | Current incomplete; raw matched source total closely. |
| `gh3015` | pass/pass | same | TUI date/season mismatch handled; FY24/FY25 source split. | Same correction and figures. | Same reasoning/source handling. |

## Chunk 089-106

| Task ID | Outcome current/raw | Behavior | Current behavior | Raw behavior | Why result differed or why same |
|---|---:|---|---|---|---|
| `m5zja8` | pass/pass | same | 20 UNGM IT tender rows with IDs, deadlines, orgs, countries, URLs. | Same 20-row scrape. | Same first-20 active IT tender requirements. |
| `qtxhi1` | pass/pass | different | Declined exact UPS cost because only states were provided; noted ZIP/dimensions required. | Assumed Dallas 75201 to NYC 10001, 4 lb package, quoted UPS Ground `$22.05`. | Both handled under-specification explicitly. |
| `r5l8a7` | pass/fail | different | Used Viator snapshot and saved three supported Orlando tours. | Listed tours, but saved Viator page was DataDome/403. | Raw claims unsupported; current grounded. |
| `s3kkv9` | pass/pass | same | VRBO Orlando private-pool search, Apr 10-15 2027, top three ratings. | Same filters/date adjustment and ratings. | Same deliverable. |
| `pvs7hz` | fail/pass | different | Could not access Wikiwand timeline AI view; substituted article-text events. | Opened timeline view and saved 67 generated timeline events with screenshots/raw text. | Raw captured requested source; current drifted. |
| `taxp0w` | pass/pass | same | WINDTRE fiber/FWA offer cards. | Same with supporting screenshots. | Same telecom package extraction. |
| `qewaou` | pass/pass | same | 30 Seven Springs LLC Westchester land-record rows, 17 unique details. | Same plus normalized outputs. | Same land-record collection. |
| `xsfh4g` | pass/pass | same | XP event 1485844 and no available ticket listings, backed by alert/API evidence. | Same API verification. | Same no-ticket result. |
| `q85jsg` | fail/fail | different | Reported Yad2 ShieldSquare/PerfDrive blocking and returned zero listings. | Worked around via indexed pages and output listings, but artifacts showed blocking/unsupported sources. | Neither provided supported extraction from requested page. |
| `swebnv` | fail/pass | different | Yelp DataDome/403 blocked both searches; empty business arrays. | Used Yelp embedded data and saved 20 audited Houston businesses without website links. | Raw found usable embedded data; current stopped at blocker. |
| `objlzy` | fail/pass | different | Arabic YouTube homepage, but no 8-12 trending video table. | Used YouTube Charts Egypt daily top music videos, 12 rows. | Raw supplied chart-backed substitute. |
| `ze5r5u` | pass/pass | same | 23 Zillow Brentwood FRBO rentals and notepad output. | Same. | Same Zillow plus notepad workflow. |
| `1b0z3n` | pass/pass | same | Answered `Government in the United States` by Claudius O. Johnson. | Same answer. | Same Quote Investigator evidence. |
| `6dpbhs` | pass/fail | different | Answered `Walter Allen` from Archives West evidence. | Runner exited with no final/artifacts after context exhaustion. | Raw runtime/context failure. |
| `2vxyzx` | pass/pass | same | Compared iPhone 16 Pro Max availability/prices; noted no separate "iPhone 16 Max." | Same interpretation/comparison. | Same retailer comparison. |
| `jgzlma` | fail/fail | different | Amazon 20 and Galaxus 20; Kaufland 0 due block and Galaxus drift. | All three arrays, but Galaxus used wrong `.ch` scope and drifted. | Both missed fully grounded three-platform top-20 supplement extraction. |
| `r1dwwo` | pass/pass | different | 44 London theatre production entries. | 60 entries with fuller venue coverage. | Both passed; raw more comprehensive. |
| `t9moge` | pass/pass | same | MARC Camden Line stops and NeighborhoodScout crime indexes. | Same plan with audit artifact. | Same transit/crime-index requirements. |

## Synthesis

### Where Raw Is Behaviorally Stronger

Raw Codex is better at stubborn recovery and broad extraction:

- Alternate official sources after blocks: `up8ijl`, `he7ur2`.
- Long pagination/count completion: `sw2292`, `pvs7hz`, `r1dwwo`.
- Dynamic UI recovery: `3rzsj4`, `yjb1qx`.
- Using adjacent public surfaces when a first surface is missing: `objlzy`, `swebnv`.

### Where Current Is Behaviorally Stronger

Current is better at grounding and not over-claiming:

- Rejects or limits blocked data instead of inventing: `55j2o7`, `r5l8a7`, `q85jsg`.
- Keeps source evidence for small factual answers: `e7flqh`, `6dpbhs`.
- Preserves better evidence trails for dynamic pages: `3d5iqv`, `iogxpb`, `trp0j4`.
- Finds robust pagination routes in some structured sites: `3w5uzb`.

### Actual Answer

The two systems are comparable in aggregate score, but they are not the same behaviorally.

Current behaves like a more controlled, more evidence-oriented agent. Raw behaves like a more adventurous browser operator that sometimes solves harder blocked/dynamic tasks but also sometimes creates unsupported or scope-drifted outputs.

The ideal product path is not to replace current with raw. It is to keep current's grounding discipline and add raw's recovery patterns:

1. more alternate-source search after blockers,
2. more long-extraction count verification,
3. stronger pagination retries,
4. pre-final grounding checks for every claimed value,
5. explicit failure if values exist only in the final answer and not artifacts/tool output.
