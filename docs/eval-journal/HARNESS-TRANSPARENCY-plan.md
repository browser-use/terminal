# Harness transparency — making the terminal browser-harness-like

Three live experiments (sub-agents driving browser-harness on our cloud browsers, tasks chosen to
match our forensics failure modes) + full read of the browser-harness source (494-line core).

## What the experiments proved (all three, independently)
1. **Source-reading is the diagnosis mechanism.** scroll() read → wheel-target + sign convention
   understood (reviews task); click_at_xy read → coordinate-space match confirmed, JS-measured
   centers fed directly (SMTCL); fill_input read → char-doubling bug root-caused, docstring
   rationale falsified on the live site (DNA). Every hard failure was resolved by reading the
   implementation, not by retrying.
2. **Our forensics failure modes vanish under this workflow.** The new-tab trap that cost our agent
   5 turns (task 3 / H5) was PRE-EMPTED (rect dump showed target=_blank before clicking). Silent
   click misses: 1-turn diagnosis via elementFromPoint vs our blind retry ladder.
3. **Self-extension works via a FILE, not a persistent kernel.** agent_helpers.py is importlib'd
   into globals on every run; helpers written once were reused across separate invocations, edits
   hot-reload, state survives compaction because it's on disk. One agent's helper (verify-after-
   write) caught a later unrelated failure. Calibration: agents skip the file for short tasks
   (heredoc cheaper) — it pays on long/crawl tasks.
4. **Interaction skills are nearly irrelevant** (scrolling.md is ONE sentence). The value is
   source + experiment + readback, plus ~700 tokens of behavioral heuristics (SKILL.md's
   "what actually works"/"gotchas"). Our always-on skills bundle was aiming at the wrong organ.
5. The winning loop, observed repeatedly: screenshot to ORIENT → js() to MEASURE →
   act → screenshot/readback to VERIFY. Plus verify-after-write on every input.

## Build plan (branch: harness-transparency, from main)
1. **Split browser_script_helpers.py (2,073 lines) into:**
   - `actions.py` (~550 lines): model-facing primitives WITH why-comments (cdp, js, goto_url,
     page_info, tabs, click/keys/fill, screenshots, scroll, waits, http_get/browser_fetch thin
     wrappers, upload, emit/note). Style: browser-harness (every helper documents its failure
     modes inline). This file is WRITTEN INTO THE SESSION CWD at start.
   - `_runtime.py` (hidden): bridge/retry, secrets/TOTP, email, nav-policy, fetch-proxy client,
     domain-skills plumbing — imported by actions.py, never surfaced.
2. **agent_helpers.py**: empty agent-editable file in cwd, importlib'd into every browser_script
   run's globals after the prelude (15 lines). Prompt line: "for repeated mechanics, write a
   helper into agent_helpers.py with a verify-after-write check; it persists across calls."
3. **Tool description → ~600 tokens**: usage shape + "the implementation is ./actions.py — read it
   when behavior surprises you" + the heuristics/gotchas block (ported & adapted: orient/measure/
   act/verify loop, readback-verify, new-tab anticipation, our cloud-browser specifics).
4. **Skills**: drop the always-on bundle AND the stdin-injection contraption from prompt-diet;
   ship the 5 non-stub skill files as plain files in cwd; one index line in the description.
5. Keep ours, don't port theirs: our fill_input (select-all+Backspace+insertText) does NOT have
   bh's char-doubling bug; the lesson is readability, not their code.

Token math: description 3.5k→0.6k always-on; actions.py ~5k read ONCE per session that needs it
(cached thereafter). Net per-turn fixed cost DOWN, with capability UP. The play is fumble
elimination (RETRY/FUMBLE = 18% of turns / 20% of tokens in the 94-run accounting) — turn
elimination is the one proven cost lever.

Gate: full eval + locked judge, score ≥93, watch the fumble-class turns specifically.

## Incidental upstream findings (browser-harness repo, worth PRs someday)
- BU_AUTOSPAWN=1 (headless remote bootstrap) documented only in run.py comments — SKILL.md's
  remote recipe cannot bootstrap on a headless box without it. All 3 agents hit this.
- fill_input double-insertion: keyDown-with-text + explicit char event = every printable char
  doubled on at least dna.fi. Agent-verified minimal repro: one press_key("5") → "55".
- stop_remote_daemon exits 2 on success (IPC connection dies with the daemon).
- drain_events() one-shot buffer is lossy across invocations; performance.getEntriesByType(
  'resource') is the robust in-page alternative for request forensics.
