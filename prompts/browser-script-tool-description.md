Run Python for browser page interaction through the Rust-held CDP connection.

This is the browser interaction tool and page/data-plane tool. Use it for navigation, page inspection, clicks, typing, scrolling, screenshots, downloads, uploads, network inspection, extraction, browser-backed verification, artifacts, and final answers.

Use the `browser` tool for connection/runtime work first. If the browser is not connected, run `browser status --json` and then an explicit connect command such as `browser connect local`, `browser connect managed --headless`, or `browser remote start`.

Important execution model:

- Each `browser_script` call starts a fresh Python process; Python variables do not persist across calls. Browser/CDP state persists in Rust.
- Fast calls return their final result immediately. Long calls return `status: running` with a `run_id`; keep observing that same run until it finishes, fails, or is cancelled.
- To listen to a running script, call this tool with `action="observe"`, the `run_id`, and optionally `observe_timeout_ms`. Prefer coarse waits (30000-120000 ms) for long navigation/extraction; do not burn many turns polling with short waits. To stop a run, call `action="cancel"` with the `run_id`; partial images/artifacts emitted before cancellation are preserved.
- A failed call may include a short diagnosis. Read it first: if it says the browser is still connected or the same page is usable, continue from the same page instead of reconnecting.
- Helpers are preimported; no imports needed for normal browser work. CDP is the source of truth — if a helper is incomplete, use `cdp(...)` directly.
- Keep browser actions sequential and deliberate. Do not import Playwright, Selenium, or Pyppeteer.

Preimported helpers:

```python
cdp(method, session_id=None, **params)
cdp_batch(calls)
js(expression_or_function_source, *args, target_id=None, returnByValue=True)

current_datetime()
new_tab(url="about:blank")
goto_url(url)
page_info()
nav_policy(url=None)

capture_screenshot(...)
screenshot(label="screenshot", full=False)
screenshot_clip(label, x, y, width, height)

click_at_xy(x, y)
fill_input(selector, text, clear=True, timeout=3)
type_text(text)
press_key(key, modifiers=0)  # accepts chords like "Meta+A"; modifiers: Alt=1, Ctrl=2, Meta/Cmd=4, Shift=8
scroll(x=0, y=600)

wait_for_load(timeout=3)
wait_for_element(selector, timeout=3, visible=False)
wait_for_network_idle(timeout=3)

current_tab()
list_tabs()
switch_tab(target_id)
ensure_real_tab()

upload_file(...)
drain_events()
email_address()
email_inbox(limit=20, sent_after=None)
email_message(message_id)
http_get(url, **kwargs)
browser_fetch(url, **kwargs)

copy_artifact(path, kind="file")
emit_output(value, label=None)
emit_image(path, label=None)
artifact_root()
outputs_dir()
session_metadata()
audit_artifact(data=None, **requirements)
load_agent_helpers()
agent_workspace()
domain_skills_for_url(url_or_domain, include_content=False)
last_domain_skills(include_content=False)
```

Usage guidance:

- First navigation should usually be `new_tab(url)`, not `goto_url(url)`, because `goto_url(url)` mutates the current controlled tab. Both send the CDP navigation command, perform a bounded readiness check, and emit a labeled `navigation` output with `status`, `page_info`, `page_state`, and `next_step`. If that output says `navigation_ready` and `page_info.url` is the expected page, trust it and inspect/extract instead of navigating again. If you chain more work after navigation, explicitly wait or poll for the specific selector/state before reading/clicking.
- If a navigation is blocked by the user's `/domains` policy (the error says so), call `nav_policy()` to see allowed/denied sites and plan within them; pass a URL (`nav_policy("example.com")`) to check before navigating. If the task can't be done within the policy, tell the user which site is blocked and suggest `/domains` or adjusting the task — don't keep retrying the blocked host.
- Keyboard semantics: `press_key(...)` simulates physical keys/shortcuts; `type_text(...)` inserts/pastes text into the focused element via `Input.insertText`. Do not combine `Input.dispatchKeyEvent` carrying printable `text` with a manual `char` event for the same character; that double-inserts in Chrome.
- For React/Vue/Svelte/controlled inputs, prefer `fill_input(selector, text, timeout=...)` over direct DOM value assignment. It focuses, clears with Cmd/Ctrl+A plus Backspace, types through physical key events, then fires final `input`/`change` events. Use stable selectors from labels, ids, names, placeholders, or visible DOM inspection; avoid brittle positional selectors like `input:nth-of-type(2)` unless you just verified that exact selector on the current page.
- If the task is site-specific, call `domain_skills_for_url(url, include_content=True)` before inventing selectors, private API routes, or flows. `goto_url(url)` also returns matching `domain_skills` metadata when a skill root is available.
- Be patient with loading pages: make several cheap observations, not one long blind wait. Prefer short waits like `wait_for_load(1)`, `wait_for_element(selector, timeout=2)`, or `wait_for_network_idle(2)`, then inspect again. A wait returning false is not a task failure; inspect the current page and continue from the best available state or decide whether it is stuck.
- Use screenshots as labeled temporal checkpoints: initial load, before/after meaningful clicks, scrolls, route changes, dialogs, uploads, downloads, and final verification. The common call is `screenshot(label)`, e.g. `screenshot("before_submit")`. For screenshot/visual-output tasks, verify the saved image is contentful and nonblank before `done`.
- Screenshot/image artifacts are sent as `input_image` content to the next model turn (the user does not see those pixels inline in the terminal; describe what you see or give the saved artifact path when asked). They are delivered even when the script then fails, and `observe` returns them as soon as they are available on a running script — use the pixels to guide the next smaller retry.
- Use `emit_output(value, label="...")` for structured observations the next turn may need (e.g. `page_info()`, extracted rows, selected DOM state, API responses). The full value stays model-visible.
- When a script emits labeled structured output, add a `# browser_summary:` JSON comment block at the top mapping each emitted label to its compact transcript summary; the runtime parses the whole script before execution. Summary values may be literals, JSONPath-like selectors such as `$.url`, or template strings such as `Read ${$.length} employee rows`. Missing summary specs fall back to a generic `Recorded <label>` summary while preserving the full output.
- Prefer this pattern over printing page or extraction objects:

```python
# browser_summary:
# {
#   "page_info": {
#     "kind": "page",
#     "url": "$.url",
#     "title": "$.title"
#   },
#   "employee_rows": {
#     "kind": "extracted",
#     "message": "Read ${$.length} employee rows"
#   }
# }

info = page_info()
emit_output(info, label="page_info")

rows = [{"name": "Ada"}, {"name": "Grace"}]
emit_output(rows, label="employee_rows")
```

- Keep `print(...)` for short debug/status text only; do not print large page, DOM, network, or extraction objects when `emit_output(...)` can carry the full value.
- Prefer coordinate clicks for visible UI: screenshot, inspect pixels, `click_at_xy(x, y)`, wait, screenshot again. Use `js(...)` for DOM inspection and raw `cdp(...)` for lower-level actions; pass JSON-serializable Python values into JavaScript with `js(function_source, *args)`, and use `target_id=` for iframe targets.
- For real user forms, act like a browser user: screenshot, click the visible field/control, type with `type_text(...)`, `press_key(...)`, or `fill_input(...)`, then screenshot or otherwise verify. Use coordinate clicks for checkboxes, radios, buttons, dropdowns, and custom controls. Do not assign `element.value`, `element.checked`, `selectedIndex`, React private state, or MutationObserver restore loops on live forms. Do not synthesize `input`, `change`, `click`, or keyboard events in page JavaScript to make a form look filled. Those anti-patterns can desynchronize framework state from the visible DOM.
- Use `http_get(...)` for one static page/API URL after the browser reveals a stable endpoint. Use `browser_fetch(...)` when the page's cookies, auth headers, or browser session are needed. Returned bodies are strings by default, bytes with `binary=True`, and expose `.status_code`, `.headers`, `.url`, `.text`, `.content`, and `.json()`. If direct HTTP hits bot or login protection, retry with `browser_fetch(...)`, site-specific headers/cookies, or the configured Browser Use fetch proxy. Do not replace source completion with blind bulk fetching; use small inspected chunks with progress, counts, missing fields, and source coverage.

- Extract only fields needed for the task. Do not emit full profile text, full DOM text, cookies, localStorage, or entire app caches unless you are debugging and the smaller field-level extraction failed.
- Save complete generated result files under `outputs_dir()` or relative paths in the cwd — files written there are collected as artifacts automatically (`copy_artifact(...)` is for files created elsewhere). Write large structured results to a file: if the task asks for an exact inline final format, return that content with `done(result=...)` and optionally `result_file=path`; otherwise finish with `done(result_file=path)`.
- For loops over multiple pages/items, emit short progress every item or every 2 seconds, whichever comes first (a short `print(...)` line or compact `emit_output(..., label="progress")`). Prefer bounded chunks with per-item micro timeouts and checkpoints written to files; inspect progress after each chunk, and if a chunk fails with a usable-page diagnosis, shrink the next chunk and resume from the last checkpoint.
- For audits after a large result, run a small independent sample/count/schema check, then repair the specific gaps it finds until the required rows/fields are complete or the run is nearly out of turns. Do not rerun the whole crawl or full detail scrape just because counts fluctuate or some pages are intermittently empty; target the missing items, and mark a gap as a genuine absence only after checking its correct source path.
- For list/profile extraction, filter the candidate list before navigating when the list page already has enough information (e.g. employee versus contractor); do not visit rows that cannot affect the final answer. Poll until the record itself is ready before extracting fields; if a loaded record is missing a required field, inspect the correct source path before marking it absent — do not record required values as missing just because the first record view is null.

Signing in / sign-ups: before signing up with a new email, check whether you're already logged in (you often drive the user's own profile) or have a saved credential for the site (listed under "Saved credentials") — if so, use it. If there's no existing login, ask the user whether to sign in with their own account (they save it via `/secrets`) or have you create a disposable account (you generate a throwaway inbox with `email_address()` and read its verification emails yourself), and wait for their choice. For the disposable path, call `email_address()`, record whatever context you need before submitting (`current_datetime()["utc"]`, existing `message_id`s from `email_inbox()`, or both), fill the email field, submit, then inspect/poll `email_inbox(sent_after=...)` or compare `timestamp`/`message_id` yourself (newest-first; `preview` already holds the code; `email_message(message_id)` has the full `text`/`html` for magic links).

CRITICAL for emailed codes — do the submit, inbox read, and code fill in ONE browser_script call. Each call is a fresh Python process and loses your variables, so a code you read in one call is gone in the next; if you split it you'll end up typing a fabricated default. In that single script, decide how to prove the email is new: for example `started_at = current_datetime()["utc"]`, then after submit poll `email_inbox(sent_after=started_at)` and verify the message `timestamp`; or snapshot old `message_id`s and compare. Extract the digits from a message you verified is current, type them into the code field, and submit. Never type a code you didn't just read from the inbox this call; a value like `123456` or `000000` is a placeholder/guess, not a real code — if you can't read one, say so instead of submitting a guess.

Do not call runtime-management helpers here. There is no `browser_connect`, `browser_status`, `browser_doctor`, or `browser_recover` helper in this tool. Those are intentionally only in the `browser` tool so the model can reason about browser lifecycle explicitly.
