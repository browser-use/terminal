# Master idea list — everything, in one place

Supersedes-and-consolidates: [`browser-architecture-first-principles.md`](./browser-architecture-first-principles.md) (analysis), [`browser-convergence-proposal.md`](./browser-convergence-proposal.md) (architecture proposal), and the eval-driven fixes from [`eval-journal/regression-88-to-81-analysis.md`](./eval-journal/regression-88-to-81-analysis.md). Every idea carries an **evidence tag**:

- `EVAL` — demanded by measured failed tasks (highest confidence, attributable point recovery)
- `REF` — proven by a working reference implementation (browsercode / browser-harness / codex)
- `FP` — first-principles / beyond what any reference does (highest upside, least proven)

Sizes: S = hours–1 day, M = days, L = week+.

---

## A. Eval-demanded fixes — what the 19 failed tasks actually point at

The data: 19 CUR failures = ~8 strategy, ~4–5 infra, 2 audit-caused, 5 judge/dataset noise. These items recover measurable points; everything in B is justified by other reasoning, not by these failures.

| # | Idea | Evidence | Tasks | Size |
|---|------|----------|-------|------|
| A1 | **Prompt: remove the "at most one targeted repair pass" ceiling**; restore the OLD global-deadline guard + hard completeness mandate ("open every required source page before declaring unavailable") | EVAL (C7/H2) | 32 | S |
| A2 | **Prompt: restore visual-first + visual-fallback discipline** — drop the "use scripts to make completion faster" front-load; add "if a script/fetch path fails or returns empty, fall back to navigating and reading the rendered page before giving up" | EVAL (C7/H1) | 38, 39, 47, 67 | S |
| A3 | **Terminal-completion barrier fix** — a turn ending on plain text must still capture/finalize the result (in flight on this branch; verify it covers the plain-text path, not just crashes) | EVAL (C2/H4) | 1, 4 | M |
| A4 | **Provider-error retry/resume** — one `stream_error: provider error` must not hard-fail a session | EVAL | 22 | S |
| A5 | **Stop fatal-izing transient errors** — a target-site read-timeout/HTTP error must never surface as fatal CDP/"browser-closed" | EVAL (C8/H3) | 26 (+21, 41, 72 amplification) | M |
| A6 | **Revert the observe min-clamp 30s → 1s** (or remove the minimum entirely) | EVAL (C5) | contributes to 1, 4 | S |
| A7 | **Fix the done-audit**: null-for-genuinely-unavailable fields are not "placeholders"; `result_file` gets the same checks as `result`; add a scope/credibility guard so unsatisfiable specs don't induce padding (task 88) | EVAL (C3/H5) | 53, 94 | S–M |
| A8 | **Session-lifetime browser lease** (claim on connect, release on session end) + demote browser activity events from `Durability::Barrier` to async journal writes — removes ~3,700 sync writes/run and the lease-contention failure surface | EVAL (C1/H3) + REF | 21, 26, 41, 72 thrash; 5× tool-failure rate | M |
| A9 | **Raise/remove the 4 KB inline browser_script stdout cap** if re-scrape thrash persists after A1–A8 | EVAL (C6, MED conf.) | indirect | S |
| A10 | **Judge/dataset hygiene** — pin judge knowledge-date per run (93 clock-drift), flag impossible specs (87, 88), re-examine ambiguous entities (9, 33). 5 of 19 failures are unrecoverable by any harness change | EVAL | 9, 33, 87, 88, 93 | M |

**Expected from A alone: 81 → ~88–90.** A1–A2 are the cheapest points in the entire document.

---

## B. Convergence architecture — reach the reference-class harness

### B-I. Events (flagship quick win — works today, no warm runtime needed)

| # | Idea | Evidence | Size |
|---|------|----------|------|
| B1 | **CDP event subscription in `CdpDispatcher`**: ring buffer (~500 events, monotonic seq) + waiter registry; stop discarding id-less messages (`browser-use-browser/src/lib.rs:~4026`). The dispatcher is the persistent object — events survive ephemeral Python processes | REF (browser-harness daemon ring buffer; browsercode `session.waitFor`) | M |
| B2 | **Bridge verbs** `{kind:"wait_event"}` (parks until match or timeout, no polling) and `{kind:"drain_events"}` | REF | S |
| B3 | **Python helpers**: `wait_for_event(method, predicate, timeout)` (predicate evaluated Python-side, re-wait loop), `drain_events(methods)` | REF | S |
| B4 | **Event-driven high-level waits**: `wait_for_download` (setDownloadBehavior + `Browser.downloadProgress state=completed` → auto-register artifact), `wait_for_navigation` (`Page.loadEventFired`), event-driven `wait_for_network_idle` (browser-harness algorithm minus the polling), `wait_for_dialog` | REF | S |
| B5 | **Cross-script event capture** — because the buffer is dispatcher-resident, script A can trigger and exit; script B drains the completion event. Beyond what browsercode can do | FP (free by-product of B1) | — |
| B6 | **Skill section teaching the event pattern** (subscribe-then-act, enabled-domains caveat, drain forensics) — capability is dead weight if untaught | REF | S |

### B-II. Prompt economics

| # | Idea | Evidence | Size |
|---|------|----------|------|
| B7 | **Skill-on-demand**: shrink the permanent browser prompt surface ~6.3k → ~0.5k tokens. ~10-line tool description; one browser SKILL.md loaded once (skills mechanism or auto-inject-on-first-use context message); 18 interaction-skills become files read when stuck; `browser-agent-system.md` shrinks to genuinely-global lines | REF (browsercode: ~10-line description + on-demand SKILL.md) | M |

### B-III. Execution substrate & tool surface

| # | Idea | Evidence | Size |
|---|------|----------|------|
| B8 | **Warm per-session Python runtime** — evolve `browser-use-python-worker` (long-lived, per-session persistent namespaces, already imports browser helpers) to host browser snippets; kill per-script interpreter spawn + per-script TCP bridge + base64 prelude | REF (browsercode in-process; browser-harness warm daemon) | L |
| B9 | **Persistent bridge** established once per session to the session's `CdpDispatcher`; `cdp()` keeps its signature | REF | (in B8) |
| B10 | **Per-snippet cancellation that does not hard-restart the worker** (today's timeout path kills all namespaces, `python-worker/lib.rs:331`); explicit agent-session ↔ browser-session ↔ namespace mapping | REF | (in B8) |
| B11 | **Single `browser_execute(code, timeout?, description)` tool** — synchronous to completion, 60s default / 10min max; delete the ~50-subcommand `browser` control-plane tool; connection becomes snippet-driven helpers (`connect_local()`, `connect_managed()`, `connect_cloud(profile=)`, `browser_status()`, `list_profiles()`); user-facing ops (doctor, profile UI) move to TUI slash-commands, off the model's budget | REF (browsercode tool shape) | M |
| B12 | **unified_exec-style continuation handle for the long tail** — snippet outliving its yield window returns partial output + a handle; model re-calls to continue. Codex's native idiom (`unified_exec` ProcessStore + `yield_time_ms`), so parity-correct; replaces bespoke observe/cancel verbs | REF (codex) | M |

### B-IV. Error doctrine

| # | Idea | Evidence | Size |
|---|------|----------|------|
| B13 | **Stale-session auto-reattach** in the dispatcher — on `"Session with given id not found"`, re-attach to first real page and retry once (port browser-harness `daemon.py:352-356`). The *only* silent auto-heal | REF | S |
| B14 | **Raw-error pass-through** — delete the diagnosis taxonomy (`browser_usable:false`, five `recover *` commands); transient-vs-fatal honesty stays as error *text* (A5), not orchestration; recovery recipes live in the skill as runnable code | REF (browser-harness "no manager layer" doctrine) | M |

### B-V. Vision

| # | Idea | Evidence | Size |
|---|------|----------|------|
| B15 | **Response tap → automatic screenshot attach**: `on_call_result` filtering `Page.captureScreenshot` → base64 → image attachments on the tool result; retires the JPEG-file + stdout-marker dance. Keep serial `view_image` (codex parity, D-DIV-3) | REF (browsercode `browser-execute.ts:204-219`) | S (after B1) |
| B16 | **`screenshot(max_dim=)` downscale** for LLM size limits (port browser-harness `helpers.py:269-281`); document the device-pixel vs CSS-pixel contract in the skill | REF | S |
| B17 | **Frame capture re-homed** per-session, flag-gated, default-off in evals (browsercode has none; it's an observability luxury, not a task-solving need) | FP | S (after B8) |

### B-VI. Knowledge accretion (convention, not machinery)

| # | Idea | Evidence | Size |
|---|------|----------|------|
| B18 | **Accretion doctrine in the skill**: write reusable helpers to `<workspace>/agent_helpers.py` (auto-load exists); file domain skills to `<workspace>/domain-skills/<host>/*.md` after figuring out something non-obvious; use ordinary file tools — no authoring tool, no registry | REF (browsercode workspace-as-plain-code; browser-harness 97 agent-authored skills) | S |
| B19 | **Verify the read loop end-to-end** — `goto_url` surfacing domain-skill filenames, `agent_helpers.py` auto-load on change — and teach it in the skill | REF | S |
| B20 | **Per-project workspace default** (browsercode `.bcode/` model: git-tracked, shareable), home fallback | REF | S |
| B21 | **Seed domain skills by running real tasks** against 2–3 complex sites and letting the agent file its own skills (never hand-author — browser-harness rule) | REF | M (run time) |

---

## C. Beyond parity — exceed the references (FP, the upside bets)

| # | Idea | Why no reference has it / why it matters | Size |
|---|------|------|------|
| C1 | **Auth story**: detect auth state *before* burning turns (cookie/landing-page probe on connect); profile reuse policy (suggest the profile that's logged into the target domain); cloud-profile handoff; explicit 2FA/captcha → ask-user protocol with a resumable checkpoint. Every reference punts ("ask the user"); auth walls are where real-world tasks die (eval 87 died on a sign-in wall) | L |
| C2 | **Parallel browsing fan-out** — multiple targets/tabs driven concurrently for embarrassingly-parallel work (check N product pages). All references serialize one browser; the dispatcher + sessionId routing already supports multiple attached targets. Needs: per-target session affinity, output interleaving rules, prompt doctrine for when to fan out | L |
| C3 | **Observation token-efficiency policy** — explicit budgets for what enters context: DOM-dump truncation strategy (head/tail like codex exec), screenshot count/size budgets per turn, vision-vs-text decision doctrine in the skill. Quietly dominates cost and context health on long tasks; ad-hoc everywhere today | M |
| C4 | **Browser artifacts × compaction design** — how screenshots/large extractions age out of history during auto-compaction (keep labels + paths, drop pixels; re-viewable via `view_image`). New-core has real token accounting (ahead of all references); the interaction is currently undesigned | M |
| C5 | **Eval flywheel as a first-class asset** — fast subset re-runs, auto-generated per-run failure taxonomy (the regression-analysis table as a script, not a hand-written doc), repeat-site task pairs that measure whether domain-skill accretion (B21) actually pays, judge date-pinning (A10) | L |
| C6 | **Structured-extraction hardening** — double down on the eval-proven API-first win: pagination/rate-limit-aware `http_get` patterns in the skill, in-browser fetch (site cookies/headers for free) as the default fallback ladder rung between raw HTTP and visual | M |
| C7 | **Hardening odds-and-ends from the fragility audit**: filesystem-scan timeout on domain-skill matching; defined behavior when a barrier/journal write fails mid-lease; orphaned-Python reaping (moot after B8) | S each |

---

## D. Keep — eval-proven or deliberately divergent; do not regress

- **API-first structured extraction** (fixed 8 tasks: 15, 27, 43, 59, 66, 68, 74, 75) and **failure-resilient retry after script errors** — keep both through every refactor.
- **Audit/repair discipline** — the *labeling* behavior ("not displayed / no match, not inferred"), with A7's threshold fixes.
- Codex-faithful harness core; multi-provider protocol×provider split (D-DIV-1); SQLite as write-behind sink (D-DIV-2 — barriers go, durability stays); serial `view_image` (D-DIV-3); ChatGPT backend dropped (D-DIV-5).
- Persistent `CdpDispatcher` + sessionId auto-injection; connection modes Local/Managed/RemoteCdp/RemoteCloud (broader than any reference).

## E. Rejected — decided against, with reasons

- **Embedding V8/deno_core for JS-as-CDP** — both references prove the load-bearing element is persistent-runtime + raw-protocol + accreting helpers, not JavaScript; the plumbing, not Python, is the limiter. Revisit only if post-B8 evals say otherwise.
- **Keeping the `browser` control-plane tool** — the manager-layer inversion in tool form.
- **Auto-recovery orchestration** — one silent re-attach (B13) max; the rest is agent code.
- **Skill/helper authoring tools or registries** — plain files + conventions.
- **Per-call durability barriers for browser activity** — replaced by A8.
- **`searchoff` as a regression lever** — measured inert.

---

## F. Sequencing

```
Week 0   A1–A2 (prompt reverts; cheapest points)  +  A3–A7 (stability/audit; partly in flight)
         └─ re-run real_v8 → expect ~88–90
Track A  B1→B2→B3→B4→B6 (events)            — independent, dispatcher+helpers files
Track B  B7 (skill-on-demand)                — independent, prompts/skills files
Track C  A8 → B8/B9/B10 → B11 → B12          — runtime critical path
Then     B13–B21 slot around tracks; C1–C6 are the next bets after re-measuring;
         A10/C5 (eval hygiene/flywheel) start anytime — they gate how well everything else can be judged
```

Tracks A/B/C are file-disjoint → parallelizable as separate agents. Re-run the 100-task eval after each track lands so movement stays attributable.

## G. Open decisions

1. Long-tail semantics: unified_exec handle (recommended) vs hard 10-min cap (B12).
2. Skill delivery: skills-mechanism load vs auto-inject-on-first-use (B7).
3. Workspace default: per-project (recommended) vs home (B20).
4. Frame capture: flag-gated keep (recommended) vs delete (B17).
5. Which beyond-parity bet first: auth (C1) or parallel fan-out (C2) — auth recommended; it kills more real-world tasks.
