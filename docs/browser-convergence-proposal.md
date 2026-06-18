# Browser convergence — the full proposal

Companion to [`browser-architecture-first-principles.md`](./browser-architecture-first-principles.md) (the analysis). This is the complete catalogue of everything we could implement to converge on "codex harness + browser-harness/browsercode browse layer," each item with mechanism, target files, size, and dependencies. One-time proposal; pick and sequence from here.

Sizes: S = hours–1 day, M = days, L = week+.

## 0. Summary

| # | Item | Closes | Size | Depends on |
|---|------|--------|------|-----------|
| 1 | CDP event subscription + ring buffer in `CdpDispatcher` | event-waits (downloads, navigation, dialogs, network) | M | nothing — works today |
| 2 | Event-wait bridge verbs + Python helpers (`wait_for_event`, `drain_events`) | same | S | 1 |
| 3 | Event-driven rebuilds: `wait_for_download`, `wait_for_navigation`, network-idle, dialog wait | same | S | 2 |
| 4 | Skill-on-demand: shrink prompt surface ~6.3k → ~0.5k tokens | prompt economics | M | nothing |
| 5 | Stability fixes (terminal barrier, transient-vs-fatal, observe clamp revert, audit thresholds) | eval regression | M | in flight |
| 6 | Session-lifetime browser lease + barrier demotion | lease churn (3,700 sync writes/run) | M | 5 |
| 7 | Warm per-session Python runtime + persistent bridge | execution substrate | L | 5 (6 helps) |
| 8 | Collapse to one `browser_execute`; delete `browser` control-plane tool | tool surface | M | 7 |
| 9 | unified_exec-style continuation handle for long runs | long-tail semantics | M | 7, 8 |
| 10 | Stale-session auto-reattach in dispatcher | error doctrine | S | nothing |
| 11 | Raw-error pass-through; delete recovery taxonomy | error doctrine | M | 5, 10 |
| 12 | Response tap → automatic screenshot attach | vision | S | 1 |
| 13 | Workspace/domain-skills accretion doctrine (prompt + conventions) | self-improvement | S–M | 4 |
| 14 | Frame capture re-home (per-session, flag-gated, off in evals) | observability cost | S | 7 |
| 15 | Screenshot downscale helper (`max_dim`) for LLM limits | ergonomics | S | nothing |

Two flagship tracks can start immediately and independently: **events (1–3)** and **prompt economics (4)**. Neither needs the warm runtime.

---

## 1. Event subscription — the flagship quick win

**The gap.** browsercode/browser-harness-js expose `session.onEvent(fn)` and `session.waitFor(method, predicate?, timeoutMs)` (`browser-harness-js/sdk/session.ts:156-178`): subscribe to a CDP event, get the payload back when it fires. The Python harness gets the same power from a 500-event ring buffer in the daemon (`daemon.py:187,251`) that `wait_for_network_idle` reads without polling. In new-core, `cdp_dispatcher_loop` routes responses by id and **discards every event** (`browser-use-browser/src/lib.rs:~4026`). So "click export, wait for the download to finish" is currently nearly impossible — there is no way to observe `Browser.downloadProgress` at all.

**Key insight (and why this lands now):** the `CdpDispatcher` is the persistent object — it outlives Python processes, scripts, and turns. Events captured there survive the ephemeral script runtime. The warm worker (item 7) makes this *nicer*, not *possible*.

### 1a. Dispatcher side (Rust)

In `CdpDispatcher`:

- `events: Mutex<VecDeque<BufferedEvent>>` — ring buffer, cap ~500 (browser-harness's number), entries `{ seq: u64, method, session_id, params, ts }`. Monotonic `seq` so consumers can cursor across script invocations.
- `waiters: Mutex<Vec<EventWaiter>>` — `{ method: String, session_id: Option<String>, tx: oneshot::Sender<BufferedEvent> }`.
- In the dispatcher loop, the branch that currently discards id-less messages instead: push to ring (evict oldest), then fulfill any matching waiters.
- API: `wait_for_event(method, session_id, timeout) -> Result<BufferedEvent>`, `drain_events(since_seq, filter) -> Vec<BufferedEvent>`, `events_seq() -> u64`.

Caveat to handle: events only flow for **enabled domains** on the attached session (`Page.enable`, `Network.enable`, …). The attach path already enables the core domains; `Browser.downloadProgress` additionally needs `Browser.setDownloadBehavior { eventsEnabled: true }` — that belongs in the helper (1c), not the dispatcher.

### 1b. Bridge verbs (Rust ↔ Python)

The per-request bridge protocol today has `{kind:"cdp", ...}`. Add:

- `{kind:"wait_event", method, session_id?, timeout_ms}` → parks on `dispatcher.wait_for_event`, returns the event payload (or timeout error). One blocking socket round-trip; no polling.
- `{kind:"drain_events", since_seq?, methods?}` → returns buffered events + current seq.

### 1c. Python helpers

```python
def wait_for_event(method, predicate=None, timeout=30):
    """Block until a CDP event matching `method` (and predicate) fires."""
    deadline = time.monotonic() + timeout
    while True:
        evt = _bridge({"kind": "wait_event", "method": method,
                       "timeout_ms": int((deadline - time.monotonic()) * 1000)})
        if predicate is None or predicate(evt["params"]):
            return evt["params"]
```

Predicates stay Python-side (closures can't cross the bridge); the helper just re-waits until the predicate passes or the deadline hits. Plus `drain_events(methods=None)` for after-the-fact inspection ("what network responses happened during that click?").

### 1d. Rebuilt high-level waits (all become event-driven, no polling)

- `wait_for_download(timeout=120)` — `Browser.setDownloadBehavior(behavior="allowAndName", downloadPath=artifact_root(), eventsEnabled=True)` once, then `wait_for_event("Browser.downloadProgress", lambda p: p["state"] == "completed")`; resolve `guid` → file path, auto-register as artifact. **The user's motivating example becomes three lines.**
- `wait_for_navigation(timeout=15)` — `Page.loadEventFired` / `Page.frameStoppedLoading` instead of polling `document.readyState`.
- `wait_for_network_idle(idle_ms=500)` — track in-flight requestIds from `Network.requestWillBeSent`/`loadingFinished`/`loadingFailed` via `drain_events`, exactly the browser-harness algorithm (`helpers.py:391-424`), minus its polling.
- `wait_for_dialog()` — `Page.javascriptDialogOpening` payload directly.
- Cross-script flows work because the ring buffer is dispatcher-resident: script A clicks and exits; script B drains/waits for the completion event. With the warm worker (7) the cursor lives in the namespace and this becomes invisible.

### 1e. Skill text

A short "Events" section in the browser skill (see 4): subscribe-then-act pattern, the enabled-domains caveat, `drain_events` forensics. browsercode's SKILL teaches this; ours must too or the capability goes unused.

---

## 2. Prompt economics — skill-on-demand (~6.3k → ~0.5k tokens)

**Measured today:** every prompt carries `browser-tool-description.md` (~450 tok) + `browser-script-tool-description.md` (~700) + `browser-agent-system.md` (~1,950) + 18 interaction-skills (~3,200) ≈ **6.3k tokens**, ~6% of a 100k context, before any task content. browsercode carries ~10 lines and loads SKILL.md once on demand.

**Proposal:**
- Tool description shrinks to ~10 lines: what it is, `code`/`timeout`/`description` params, "read the browser skill before first use."
- One **`browser` SKILL.md** (loaded once via the existing skills mechanism, or auto-injected as a context message on first browser tool use — same pattern as codex skill injection, `codex-rs/core/src/skills.rs`): connection ways, helper API reference, raw-`cdp()` doctrine, events section (1e), vision contract, workspace/domain-skills conventions, recovery recipes (as code).
- The 18 interaction-skills become **files the agent reads when stuck** (paths listed in the skill), not prompt preamble. browser-harness ships them exactly this way.
- `browser-agent-system.md` shrinks to the few lines that are genuinely global (the tool exists; load the skill; container semantics).

Acceptance: idle-prompt token count drops ~5.5–6k; browser task evals neutral-or-better (less mode-confusion is the expected direction, per the "strategy flight" eval findings).

---

## 3. Tool surface collapse (after warm runtime)

- One model-facing tool: **`browser_execute(code, timeout_secs?, description)`**. Synchronous to completion; default 60s, max 10min (browsercode's numbers).
- Delete the `browser` control-plane tool (~50 subcommands). Absorb into snippet-callable helpers: `connect_local()`, `connect_managed()`, `connect_cloud(profile=...)`, `browser_status()`, `list_profiles()`. Connection becomes snippet-driven, like browsercode's `session.connect()`. User-facing operations (doctor, profile management UI) move to TUI slash-commands — off the model's token budget entirely.
- Long-tail escape hatch = **unified_exec idiom** (item 9), not bespoke verbs: if a snippet outlives its yield window, return partial output + a handle; the model re-calls with the handle to continue. This is codex's native pattern (`unified_exec/mod.rs` ProcessStore + `yield_time_ms`), so it's parity-correct. `observe`/`cancel` as distinct actions disappear.

---

## 4. Warm per-session Python runtime

- Evolve `browser-use-python-worker` (already long-lived, already per-session namespaces, already imports browser helpers) to host browser snippets: one REPL process per agent session, namespace persists (imports, helper defs, variables) across calls.
- Replace the per-script TCP listener + bridge thread + base64 prelude (`lib.rs:983,994,6469`) with a **persistent bridge** established once per session, targeted at the session's `CdpDispatcher`. `cdp()` keeps its signature.
- Hard requirements: per-snippet cancellation that does **not** hard-restart the worker (today's timeout path kills all namespaces, `python-worker/lib.rs:331`); explicit agent-session ↔ browser-session ↔ namespace mapping; per-call result buffers reset while user globals persist (already the worker's behavior).
- Frame capture (item 14): re-home the 2fps thread from per-subprocess to per-session, behind a flag, default-off in evals.

## 5. Lease & durability simplification

- Browser lease becomes **session-lifetime**: claim on first connect, release on session end / explicit disconnect. Kills the per-call claim/release pair (~3,700 synchronous barrier writes per 100-task run; measured 5× tool-failure contribution).
- Browser activity events demote from `Durability::Barrier` to async journal writes. Durability (D-DIV-2 write-behind sink) is untouched; only the per-call *sync barrier* goes.

## 6. Error doctrine

- Port browser-harness's single silent auto-heal: on `"Session with given id not found"`, re-attach to the first real page and retry once (`daemon.py:352-356`), inside the dispatcher.
- Everything else: **raw error text into the tool result**. Delete the diagnosis taxonomy (`browser_usable:false`, five `recover *` commands). Transient-vs-fatal classification (item in P0) stays — but as honest error *text*, not as orchestration. Recovery recipes live in the skill as runnable code.

## 7. Vision

- With 1a's tap infrastructure, add `on_call_result` filtering `Page.captureScreenshot`: collected base64 → image attachments on the tool result, exactly browsercode (`browser-execute.ts:204-219`). Retires the JPEG-file + stdout-marker dance.
- Keep serial `view_image` (codex parity, D-DIV-3) unchanged.
- `screenshot(max_dim=...)` downscale option (browser-harness `helpers.py:269-281`) — trivially portable, avoids oversized vision payloads.

## 8. Knowledge accretion (convention, not machinery)

- No new tools. The skill teaches: write reusable helpers to `<workspace>/agent_helpers.py` (auto-load exists), file domain skills to `<workspace>/domain-skills/<host>/*.md` after figuring out something non-obvious (browser-harness rule: agent-authored only, never hand-written), use ordinary file tools.
- `goto_url` already surfaces matching domain-skill filenames — verify end-to-end and document in the skill.
- Default workspace scope: per-project, home fallback (browsercode `.bcode/` model).
- Seed by running 2–3 real tasks against complex sites and letting the agent file its own skills.

---

## 9. Sequencing

```
now ──► Track A: events (1 → 2 → 3) ──────────────┐
   ──► Track B: skill-on-demand (4) ──────────────┤
   ──► Track C: P0 stability (5, in flight) ──► 6 ──► 7 ──► 8 ──► 9
   ──► 10 (reattach) anytime; 11 after 5; 12 after 1; 13 after 4; 14 after 7; 15 anytime
```

Tracks A, B, C are file-disjoint (dispatcher+helpers / prompts+skills / runtime+entrypoint) → parallelizable as separate agents. Each track is independently eval-able; run the 100-task suite after each lands.

## 10. Explicitly rejected (unchanged from the analysis doc)

Embedding V8 for JS-as-CDP; keeping the control-plane tool; auto-recovery orchestration; authoring tools/registries for skills; per-call durability barriers.

## 11. Decisions needed

1. Long-tail: unified_exec handle (recommended) vs hard 10-min cap.
2. Skill delivery: skills-mechanism load vs auto-inject-on-first-use context message.
3. Workspace default: per-project (recommended) vs home.
4. Frame capture: flag-gated keep (recommended) vs delete.
