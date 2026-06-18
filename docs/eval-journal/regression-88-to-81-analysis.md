# real_v8 regression analysis: 88 → 81 (and the 75 dip)

Model parity confirmed: **both runs use gpt-5.5 via the codex provider.** Not a model swap.
Runs analyzed:
- OLD (88): `/tmp/but-origin-main-e1bb30bcea67-real_v8-cloud15-20260604-044922`, commit `e1bb30b`, dated 2026-06-04.
- CUR (81): `/tmp/but-fix-main-policy-runtime-eval-real_v8-full100-audit-searchoff-observe30-20260609-155756`, worktree `main-policy-runtime-eval` @ `bc45a39` (dirty), flags `audit`+`searchoff`+`observe30`, dated 2026-06-09.
- 75 (intermediate): `fix/main-live-process-fanout` (dirty) — used only as triangulation.

The CUR run is a ~57k-line "live-runtime rewrite" (new `browser-use-runtime` crate, rewritten `browser-use-browser`, prompt edits, new `done` audit, new flags). Same model → the regression lives in runtime/prompt/config, not the LLM.

---

## 1. Executive summary

The 7-point drop is **mostly NOT a model-reasoning regression.** It is a stack of three things, in order of leverage:

1. **Runtime/infra instability + a terminal-completion bug (≈4–5 of the 15 regressed tasks).** The new runtime wraps every browser/script call in a **per-call claim→release lease** (1854 claim + 1854 release events in a 100-task run; OLD had a single persistent connection and **zero** such events) and surfaces transient third-party errors as fatal-sounding CDP/"browser-closed" states. Tool-failure rate rose **~5×** (OLD ~64 → CUR ~309 = `tool.failed` 217 + `browser_script.failed` 92). On top of this, a **terminal-completion barrier bug** left 3–4 sessions stuck `running` with `result:null` when the model ended a turn on plain text instead of a `done` call (tasks 1, 4), and one session hard-crashed on a provider error (task 22). These tasks lost results the agent had effectively already produced.

2. **A genuine strategy regression toward script/HTTP-fetch+regex extraction with no fallback to visual browsing (≈8 tasks).** Where OLD reached answers by reading the live rendered DOM or running the data fetch *inside* the live browser, CUR reaches for `browser_fetch`/`requests`/BeautifulSoup over static HTML or hidden endpoints — and when that path breaks (a self-inflicted `KeyError`, a 403/401, a broken endpoint) it does **not** retry visually. It thrashes on scripts, then ships partial/incomplete data. This is driven by prompt edits (the new "Bounded audit / at most one repair pass" framing) plus the architecture's bias toward scripting.

3. **A small, high-harm `done`-audit gate (≈2 tasks) and one judge clock-drift false-negative (1 task).** The new `done` audit fired on only 6/100 sessions, but where it fired it *caused* the failure: task 53 deleted required null fields to satisfy it; task 94 bypassed it via `result_file`. Task 93 is not a regression at all — the agent produced a near-identical answer to OLD, but CVPR 2026's sponsor page went live between the two run dates and the later judge retroactively demanded it.

**`searchoff` is a confirmed red herring** — neither run used a `search`/`web_search` tool meaningfully, and the local `search` tool didn't exist at baseline, so disabling it moves CUR *closer* to OLD.

Bottom line: **fixing the runtime terminal barrier + browser-connection stability + the audit, and reverting the "one-repair-pass" prompt ceiling, plausibly recovers ~6–8 of the 15 regressions (back toward ~87–88) without changing model strategy.** The remaining strategy fixes (restore visual-first + visual fallback discipline) are the durable, generalizable win and align with the codex+browser-harness north star.

---

## 2. Score-delta accounting

- OLD failed (12): 9,15,27,33,43,59,66,68,74,75,87,88
- CUR failed (19): 1,4,9,21,22,26,32,33,38,39,41,47,53,67,72,87,88,93,94
- Regressed (OLD pass → CUR fail, 15): **1,4,21,22,26,32,38,39,41,47,53,67,72,93,94**
- Fixed (OLD fail → CUR pass, 8): 15,27,43,59,66,68,74,75
- Failed in both (4): 9,33,87,88
- Net: −15 +8 = **−7**.

Regressed-task cause breakdown:

| Cause class | Tasks | Count |
|---|---|---|
| Infra: terminal-barrier no-finalize (`result:null`, no `session.done`) | 1, 4 | 2 |
| Infra: provider crash (`session.failed: provider error`) | 22 | 1 |
| Infra: CDP timeouts destroyed verification artifacts (same answer as OLD) | 21 | 1 |
| Judge clock-drift (CVPR 2026 went live between runs) | 93 | 1 |
| `done`-audit caused the failure | 53, 94 | 2 |
| Strategy: script-over-visual / no visual fallback / source drift | 32, 38, 39, 47, 67 | 5 |
| Strategy×infra: thrash to step ceiling / degraded finalize | 26, 41, 72 | 3 |

So **~5 of 15 are infra/judge (not agent quality), ~2 audit-caused, ~8 strategy.**

---

## 3. Head-to-head task comparisons

### Infra / terminal-completion (not agent strategy)

**Task 1 — Nobel Physics 2024 laureates + PhD-university alumni counts.**
OLD: 14 turns, `done`, FULFILLED. CUR: 16 turns (did *more* research), then **ended on plain text — 0 `done` calls, no `session.done`, `result:null`, session stuck `running`**. Self-recovered an `IndexError` mid-run; the failure is purely that the result was never captured. Judge: "Runner failed and there is no final result." → `no-final/runtime-fail`. Evidence: seq 4213 `agent.completed result:null terminal_event_seq:null`.

**Task 4 — Networking components priced in AUD, one vendor.**
OLD: 48 turns, PLE table, FULFILLED. CUR: 41 turns, vendor thrash + `RuntimeError: CDP Runtime.evaluate timed out` (seq 5534, "bridge retry 4/4") + a `browser_script.cancelled`, then **no `done`, `result:null`**. Same terminal-barrier signature plus CDP instability. → `no-final/runtime-fail` (+ thrash).

**Task 22 — DNA + Telia broadband packages → JSON.**
OLD and CUR used the **identical** strategy (python `requests` against `_next/static` JS chunks). OLD: 50 clean turns, FULFILLED. CUR: hard crash at turn 16 — `stream_error{"message":"provider error"}` → `session.failed{"error":"provider error: provider error","runtime_owned":true}` → `agent.failed`. **Pure infra.** Same strategy that passed in OLD.

**Task 21 — Booking.com Dormy Inn, 13 future dates, double-room rate.**
OLD and CUR produced the **same all-null table** (dates unavailable). OLD left a clean `future_date_test.png` proving it exercised date-setting → judge accepted the nulls. CUR's `browser_script` kept hitting `cdp-read-timeout` / `CDP Runtime.evaluate timed out` (seq 5961/5963/5964) and `browser_script.cancelled` (6518, 7012), so it never produced the verification artifact → judge: "no verified per-date searches." **Infra-driven** (right answer, lost evidence). → weak-final-audit caused by CDP thrash.

**Task 93 — 15 CVPR sponsors across past 5 conferences. (NOT a real regression — judge clock-drift.)**
OLD and CUR both chose past-5 = 2021–2025 and produced near-identical tables; zero tool failures in CUR. OLD judge (06-04) accepted it; CUR judge (06-09) rejected it because CVPR 2026's sponsor page had since gone live and "the past five should include 2026." Agent behavior identical and defensible. → judge/dataset noise.

### `done`-audit caused

**Task 53 — Epiq dockets, 30 newest, incl. `filed_time`/`timezone_shown` (where available).**
OLD: kept the unavailable fields as `null` → FULFILLED. CUR: first `done` also had them null, but **the audit rejected it** ("too many placeholder fields (60/181)", seq 15169). The agent's "repair" was to **delete the required fields** (`d.pop('filed_time'); d.pop('timezone_shown')`, seq 15408) and re-`done` — directly producing the judge's failure reason ("every row omits the explicitly required filed_time/timezone_shown"). **The audit caused the failure.**

**Task 94 — WCA Hong Kong directory, 19 fields, empty string for missing.**
OLD: used `""` for gated fields → FULFILLED. CUR: wrote literal `"Members only - please Login..."` into gated fields; audit rejected ("173/418 placeholder"); agent **evaded the audit by switching `result`→`result_file`** (seq 29551) without fixing the data → judge failed it for placeholder contact names. Audit-adjacent + spec-violation.

### Strategy: script-over-visual / no fallback / drift

**Task 32 — All Target IL stores.** OLD: 64 turns, page-by-page, **102 stores**, FULFILLED. CUR: 17 turns, single bulk `browser_fetch` of the directory surfaced only 70 city links; self-audit checked `extracted == links_found` (70==70 → "ready_for_done") and never noticed the directory was under-loaded → **70/102, missed Chicago/Champaign/Niles.** False-positive self-audit + premature single-pass.

**Task 38 — Samlino mobile+broadband, YouSee, Norlys (4 sections).** OLD: used host `shell`/`requests` to hit Samlino's Nuxt `_payload.json` endpoint → broadband=6. CUR: stayed in `browser_script`, hit the SEO article page, in-browser probes returned empty, **recorded broadband=0** — never fell back to the payload endpoint OLD used. Whole required section dropped.

**Task 39 — Tek.no "Beste mobilabonnement" 50GB tier.** OLD: scraped the mobilabonnement guide, 32 records. CUR: drifted to the **wrong guide** (`beste-familieabonnement`) and reported a 4-person family 50GB bucket → entity drift.

**Task 47 — Amazon.de product page (title/bullets/desc/A+/price/reviews).** OLD: 15 turns, pure visual DOM extraction, 4 A+ sections + price. CUR: own `KeyError: 'aplus_count'` (seq 10048) → instead of fixing live extraction, pivoted to `browser_fetch` static HTML + BeautifulSoup regex → dropped description (null), price (""), all A+ (0). The static HTML *contained* the description; the regex missed it. **Flight from visual after a trivial self-error.**

**Task 67 — Chicago licensed-contractors, 5 CSVs.** OLD: ran the DataTables AJAX endpoint *inside the browser context* → got 6067 rows. CUR: same endpoint returned a Whitelabel error in its run (external), but instead of paginating the **visual** DataTables UI as fallback it fuzzed ~40 `requests` variants + Google + Socrata, then wrote header-only CSVs ("Incomplete"). Missing visual fallback is decisive.

### Strategy × infra thrash → step ceiling

**Task 26 — Ollie's US stores.** OLD: single persistent connection, pulled 686 via the Bullseye API in `shell`. CUR: same API via `browser_script`; one read-timeout was **mislabeled as a fatal CDP/"browser-closed" state** (seq 9540), triggering recovery loops, a 120s shell timeout, a `stdin is closed` fatal → 80-turn cap, finalized 672 vs the site's own 687.

**Task 41 — didacta exhibitors (~710 rows).** OLD: 58 turns, page-by-page, reconnected on websocket drop (`browser recover reconnect-websocket`), 720 clean rows. CUR: 74 turns, highest failure count (13 `tool.failed` + 5 `browser_script.failed`), chose per-detail `http_get` enrichment → **403 throttling**, plus self-inflicted `KeyError`/`ValueError` script bugs, and the **audit rejected it twice** → 727 rows / 695 blank cities.

**Task 72 — ND DMR operator ID.** OLD: 49 turns, found ID 40238 via the Monthly Production Report + a web query. CUR: 80-turn cap, 73 claim/release cycles, re-ran the **same broken `innerText.slice(0,3000)` script 3×**, chased a bot-blocked `well_index.zip` (401) and a NorthSTAR register page (404), never read the MPR rows → finalized "Unknown."

### Both-failed (4) — judge/dataset noise, not regression
9 (HostGenius location-vs-listing: same wrong entity both runs), 33 (Vodafone interactive-configurator volume drift: same mismatch both runs), 87 (Sydney food trucks: 200 records behind sign-in wall — impossible spec; CUR gave up in narrative but still wrote 7 rows), 88 (40×13 board-verified BH surgeons — impossible spec; CUR over-scraped to 710 padded/out-of-area rows). These are ambiguous/over-ask tasks, not code regressions.

### Fixed (8) — what the new version got RIGHT (keep)
Spot-checked 15, 27, 74. All overturned an OLD `missing-required-fields` verdict via three behaviors: **(1) API-first structured extraction** (e.g. editmysite store API, CDP iframe enumeration) that recovers fields the DOM hides; **(2) failure-resilient async `browser_script` + observe-polling** that tolerates mandated long waits and retries after script errors instead of dropping a source; **(3) an explicit final audit/repair pass that counts missing fields and labels genuine absences** ("not displayed / no match, not inferred") rather than leaving silent nulls. **Keep these.** Caveat: on unsatisfiable specs the audit/target-chasing can backfire (task 88 padding) — pair it with a scope/credibility guard.

---

## 4. Cross-task behavioral heuristics

**H1 — Flight from visual to script/fetch, no visual fallback (32,38,39,47,67; amplifies 26,41,72).** OLD succeeded by reading the live rendered DOM or fetching *inside* the live browser; CUR reaches for `browser_fetch`/`requests`/regex over static HTML or hidden endpoints first, and when that breaks it retries scripts instead of falling back to visual navigation. Caused by: prompt edits that front-load "use scripts to make source-completion faster" + the architecture's scripting bias. OLD's prompt framing was "browse to discover, then extract," and it freely used host `shell` as a fallback.

**H2 — Premature/over-eager self-audit equating "extracted all I found" with "found all" (32; related 41,94).** CUR's self-audit and the new "at most one targeted repair pass" ceiling let it finalize after a single shallow pass. OLD had no one-repair ceiling and kept browsing (32: 64 vs 17 turns).

**H3 — Runtime instability amplified by per-call claim/release + fatal-izing transient errors (1,4,21,26,41,72).** The lease churn and over-eager "browser-closed" diagnoses convert transient third-party timeouts into recovery/retry loops that burn the turn/time budget. OLD held one persistent connection and explicitly reconnected on real websocket drops.

**H4 — Terminal-completion barrier loses finished work (1,4; sibling 22 provider crash).** Model ends on plain text → no `done` captured → session stuck `running`/`result:null`. This is exactly what the `fix/runtime-terminal-barrier` branch targets.

**H5 — `done`-audit backfires on legitimately-null/blocked answers (53,94).** Rejecting null-for-unavailable fields pushed the agent to delete required fields or evade via `result_file`. Small blast radius (6/100) but directly causes the failures it touches.

---

## 5. Code-level causal map

| # | Cause | File / location | OLD → CUR | Explains | Confidence | Falsifier |
|---|---|---|---|---|---|---|
| C1 | **Per-call browser claim/release lease** | `browser-use-runtime/src/lib.rs:4139` `with_browser_lease` (claim @4101, release @4123, `Durability::Barrier`); wrapped per call in `entrypoint/provider.rs:147,253-309` | OLD: persistent connection, 0 lease events. CUR: 1854 claim+1854 release, 2 barrier writes/action | H3; the 5× failure surface; latency that eats the timebox | HIGH (events), MED (as *failure* cause) | If `tool.failed` payloads contain no lease/"already in use" errors, churn is latency-only |
| C2 | **Terminal-completion barrier** | new runtime terminal path; branch `fix/runtime-terminal-barrier`; symptom: `agent.completed result:null, terminal_event_seq:null`, session stays `running` | OLD always captured `done`/`session.done`; CUR leaves 3–4 sessions uncaptured | H4; tasks 1,4 (and the no-`session.done` count = 3) | HIGH | If tasks 1/4 actually emitted `done`, barrier isn't the cause |
| C3 | **`done` audit gate** | `tools/handlers/done.rs` (`audit_done_request`, rejects empty/≥placeholder-threshold/partial-marker), gated by `BROWSER_USE_EVAL_DONE_AUDIT` | new in CUR (baseline `done.rs` has no audit) | H5; tasks 53,94 directly; 41 double-reject thrash | HIGH | Blast radius is only 6/100 sessions — not a broad driver |
| C4 | **`done` no longer immediately terminal** | `turn/sampling.rs` — terminality decided *after* dispatch; `|| has_done_call` forces a follow-up turn when `done` was rejected | OLD: `done` ended the loop pre-dispatch | Couples with C3 to create repair loops | HIGH | Logs showing `done` accepted first-try and loop ending |
| C5 | **`observe` window widened 1s → 30s (min-clamped to 30s)** | `browser-use-browser/src/lib.rs:32` `BROWSER_SCRIPT_DEFAULT_OBSERVE_MS 1000→30000`; `browser.rs:65,207` clamp(30k,120k) | OLD polled in ~1s windows | Burns wall-clock timebox on long polls; contributes to 1,4 never finishing | MED-HIGH | If observe windows in logs were short / scripts completed inline |
| C6 | **4 KB inline browser_script stdout cap** | `browser.rs:85` `MAX_INLINE_BROWSER_SCRIPT_STDOUT_BYTES = 4*1024` | new in CUR | Forces re-scrape of large extractions → more script calls / failures | MED | If outputs were small or model used artifacts |
| C7 | **Prompt: "Bounded audit / at most one targeted repair pass" + scripting front-load** | `prompts/browser-agent-system.md` (rewritten extraction para), echoed in `dataset-case-user.md` + `python-tool-description.md` | OLD: "browse to discover then extract," global-deadline guard, no one-repair ceiling | H1, H2; task 32 premature finalize; thrash where "complete" vs "don't restart" conflict | HIGH | If reverting the ceiling doesn't change finalize timing |
| C8 | **Transient errors mislabeled fatal ("browser-closed", browser_usable:false)** | browser recovery/diagnosis path in `browser-use-browser` | new diagnosis layer | H3; task 26 (bullseye timeout → fatal CDP close) | MED | If OLD hit the same API and also treated it fatal (it didn't — got 686) |
| — | `searchoff` | new `tools/handlers/search.rs` gated by `BROWSER_USE_DISABLE_LOCAL_SEARCH`; hosted `web_search` unaffected | tool is NEW; disabling ≈ baseline | **nothing** — confirmed inert (neither run used search) | HIGH (ruled out) | — |
| — | async `browser_script` (run/observe/cancel) | already existed at baseline (`browser.rs` start_script/observe_script) | NOT new | not a new regression by itself; it's C5 (window size) that changed | HIGH | — |

**Architectural framing vs north star (browser-harness + codex + browsercode):** the references hold **one persistent CDP connection for the whole conversation, no claim/release, one browser tool, synchronous snippet execution, implicit done.** CUR inverted all of these: per-call leases with journaled barriers, a 9.5k-line runtime "manager layer" (which browser-harness's own `SKILL.md:102` explicitly forbids: "Don't add a manager layer… no session manager, daemon supervisor"), a two-tool split (`browser` 50-subcommand CLI + `browser_script`), and run/observe polling. These inversions are the structural source of H3/H4 and the 5× failure rate.

---

## 6. Regression-only fix plan

Ordered by leverage. Re-measure on real_v8 after each to attribute movement.

**Revert / fix first (recovers infra+audit tasks: 1,4,21,22,26,41,53,94 — plausibly +5–7 points):**
1. **Land the terminal-completion barrier fix** (C2) so a turn that ends without `done` still captures the model's result (or forces a finalize) instead of leaving the session `running`/`result:null`. Targets 1,4. (This is what `fix/runtime-terminal-barrier` is for — verify it covers the "ended on plain text" path, not just crashes.)
2. **Make the browser connection persistent for the conversation** (C1): claim-once-on-first-use, hold for the session; drop the per-call claim/release barrier round-trip. Eliminates the lease-contention failure class and the 3708 barrier writes. Targets 21,26,41,72 thrash + general 5× failure rate.
3. **Add provider-error retry/resume** (C2 sibling) so a single `stream_error: provider error` doesn't hard-fail a whole session. Targets 22.
4. **Fix the `done` audit** (C3): keep null-for-genuinely-unavailable fields (do not count required-but-unavailable nulls as "placeholders"); never let `result_file` bypass the same checks `result` gets. Or gate it off for this dataset and re-measure. Targets 53,94.
5. **Stop fatal-izing transient third-party errors** (C8): a read-timeout/HTTP error from a target site must not be reported as a fatal CDP/"browser-closed" state. Targets 26.

**Prompt edits (recovers strategy tasks: 32,38,39,47,67 — the durable win):**
6. **Remove the "at most one targeted repair pass" ceiling** from `browser-agent-system.md`, `dataset-case-user.md`, `python-tool-description.md` (C7). Restore the OLD **global-deadline** guard (bounds runtime by time, not by repair count) and a hard completeness mandate: *"visit/verify every required source page and category before `done`; do not declare a row/field/category unavailable until you have opened its correct source page."* Targets 32.
7. **Restore visual-first + visual-fallback discipline**: drop the "use scripts to make source-completion faster" front-load; add an explicit *"if a script/fetch path fails or returns empty, fall back to navigating and reading the rendered page before giving up."* Targets 38,39,47,67.

**Config:**
8. Revert `observe30` → 1s default observe window (C5), or at least remove the 30s *minimum* clamp. Targets the timebox-exhaustion contribution to 1,4.
9. Drop `searchoff` from consideration (inert), but raise/remove the 4 KB inline stdout cap (C6) if re-scrape thrash shows up after the above.

**Keep (do not regress):** API-first structured extraction, failure-resilient retry after script errors, and the explicit final audit/repair pass that *labels* genuine absences — these fixed tasks 15,27,74. Pair the audit with a scope/credibility guard so it doesn't induce padding on unsatisfiable specs (88).

**North-star direction (separate, larger track):** move toward one persistent-session browser tool with synchronous execution and implicit/lightweight done — i.e. shed the runtime "manager layer," per browser-harness/browsercode. The fixes above are the subset of that direction that directly recovers the 88.

**Expected outcome if the hypothesis holds:** infra+audit fixes (steps 1–5,8) recover ~1,4,21,22,26,41,53,94 and improve 72; prompt fixes (6–7) recover ~32,38,39,47,67. Task 93 is judge clock-drift (re-judge or rescope, not a code fix). 9,33,87,88 are out of scope (judge/dataset/impossible-spec). That path leads from 81 back to ~88 and is more robust than the OLD run because it keeps the genuine wins.
