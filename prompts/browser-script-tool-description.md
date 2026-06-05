Run Python for browser page interaction through the Rust-held CDP connection.

This is the browser interaction tool and page/data-plane tool. Use it for navigation, page inspection, clicks, typing, scrolling, screenshots, downloads, uploads, network inspection, extraction, browser-backed verification, artifacts, and final answers.

Use the `browser` tool for connection/runtime work first. If the browser is not connected, run `browser status --json` and then an explicit connect command such as `browser connect local`, `browser connect managed --headless`, or `browser remote start`.

Important execution model:

- Each `browser_script` call starts a fresh Python process.
- Python variables do not persist across calls.
- Browser/CDP state persists in Rust.
- Fast calls return their final result immediately. Long calls return `status: running` with a `run_id`; keep observing that same run until it finishes, fails, or is cancelled.
- To listen to a running script, call this tool with `action="observe"`, the returned `run_id`, and optionally `observe_timeout_ms`. If observe reports no new output, wait/back off instead of polling in a tight loop.
- To stop a running script, call this tool with `action="cancel"` and the `run_id`. Partial images and artifacts emitted before cancellation are preserved.
- A failed `browser_script` call may include a short diagnosis. Read that diagnosis first: if it says the browser is still connected or the same page is usable, continue from the same page instead of reconnecting.
- Helpers are preimported; you do not need imports for normal browser work.
- CDP is the source of truth. If a helper is incomplete, use `cdp(...)` directly.
- Keep browser actions sequential and deliberate.
- Do not import Playwright, Selenium, or Pyppeteer.

Preimported helpers:

```python
cdp(method, session_id=None, **params)
cdp_batch(calls)
js(expression_or_function_source, *args, target_id=None, returnByValue=True)

new_tab(url="about:blank")
goto_url(url)
page_info()

capture_screenshot(...)
screenshot(label="screenshot", full=False)
screenshot_clip(label, x, y, width, height)

click_at_xy(x, y)
fill_input(selector, text, clear=True)
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
http_get(url, **kwargs)
http_get_many(urls, **kwargs)
browser_fetch(url, **kwargs)
browser_fetch_many(requests, **kwargs)

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

- First navigation should usually be `new_tab(url)`, not `goto_url(url)`, because `goto_url(url)` mutates the current controlled tab. `new_tab(url)` and `goto_url(url)` have zero implicit wait: they send the CDP navigation command and then return without waiting for readyState, network idle, selectors, paint, or sleeps. If you chain more work in the same script after navigation, explicitly wait or poll before reading/clicking. If navigation is the last action before yielding to the model, the LLM call itself may provide enough elapsed time; the next call must still inspect state before assuming the page loaded.
- Keep keyboard semantics browser-harness/Rod aligned: `press_key(...)` simulates physical keys or shortcuts, while `type_text(...)` inserts/pastes text into the focused element with `Input.insertText`.
- For React/Vue/Svelte/controlled inputs, prefer `fill_input(selector, text)` over direct DOM value assignment. It focuses the element, clears with Cmd/Ctrl+A plus Backspace, types through physical key events, then fires final `input`/`change` events.
- Do not combine `Input.dispatchKeyEvent` carrying printable `text` with a manual `char` event for the same character; that double-inserts text in Chrome.
- If the task is site-specific, call `domain_skills_for_url(url, include_content=True)` before inventing selectors, private API routes, or flows. `goto_url(url)` also returns matching `domain_skills` metadata when a skill root is available.
- Be patient with loading pages by making several cheap observations, not one long blind wait. Prefer short waits such as `wait_for_load(1)`, `wait_for_element(selector, timeout=2)`, or `wait_for_network_idle(2)`, then inspect again. If a wait returns false, that is not a task failure; inspect the current page and continue from the best available state or decide whether it is stuck.
- Use screenshots as labeled temporal checkpoints when visual state matters: before/after meaningful clicks, scrolls, route changes, dialogs, uploads, downloads, and visual final verification. For text-heavy research, document reading, search, pricing, tables, and list extraction, prefer `page_info()`, `js(...)`, targeted DOM text, `http_get_many`, or `browser_fetch_many`; screenshots add latency and usually do not help.
- The common screenshot call is `screenshot(label)`, for example `screenshot("before_submit")`.
- Screenshot/image artifacts are sent as `input_image` content to the next model turn. The user does not see those pixels inline in the terminal; describe what you see or provide the saved artifact path when the user asks for the screenshot.
- If a script emits screenshots/images and then fails, the next model turn still receives the images alongside the failure diagnosis. Use those pixels to decide the next smaller retry.
- If a running script emits screenshots/images before it finishes, `observe` returns those images as soon as they are available. Use the pixels to guide the next observe/retry.
- Use `emit_output(value, label="...")` for structured observations that the next model turn may need, such as `page_info()`, extracted rows, selected DOM state, or API responses. The full value stays model-visible.
- When a script emits labeled structured output, add a `# browser_summary:` JSON comment block at the top of the script that maps each emitted label to the compact transcript summary. Write the code/labels first mentally, then place or update this block before submitting the tool call; the runtime parses the whole script before execution.
- Summary values may be literals, JSONPath-like selectors such as `$.url`, or template strings such as `Read ${$.length} employee rows`. Missing summary specs fall back to a generic `Recorded <label>` summary while preserving the full output.
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

- Keep `print(...)` for short debug/status text only. Do not print large page, DOM, network, or extraction objects when `emit_output(...)` can carry the full value.
- Prefer coordinate clicks for visible UI: screenshot, inspect pixels, `click_at_xy(x, y)`, wait, screenshot again.
- Use `js(...)` for DOM inspection and raw `cdp(...)` for lower-level browser actions.
- Use `js(function_source, *args)` when passing JSON-serializable Python values into JavaScript; use `target_id=` as a keyword for iframe targets.
- For real user forms, act like a browser user: screenshot, click the visible field/control, type with `type_text(...)`, `press_key(...)`, or `fill_input(...)`, then screenshot or otherwise verify. Use coordinate clicks for checkboxes, radios, buttons, dropdowns, and custom controls. Do not assign `element.value`, `element.checked`, `selectedIndex`, React private state, or MutationObserver restore loops on live forms. Do not synthesize `input`, `change`, `click`, or keyboard events in page JavaScript to make a form look filled. Those anti-patterns can desynchronize framework state from the visible DOM.
- Use `http_get(...)` for one static page/API URL after the browser reveals a stable endpoint, and `http_get_many(...)` for several independent public URLs. Use `browser_fetch(...)` or `browser_fetch_many(...)` when the page's cookies, auth headers, or browser session are needed. Returned bodies are strings by default, bytes with `binary=True`, and expose `.status_code`, `.headers`, `.url`, `.text`, `.content`, and `.json()` for convenience. `browser_fetch(...)` and the batch helpers return error records by default so one bad endpoint does not waste the whole extraction chunk; pass `return_error=False` or `return_errors=False` only when a hard failure is intended. If direct HTTP hits bot or login protection, retry with `browser_fetch(...)`, site-specific headers/cookies, or the configured Browser Use fetch proxy.
- Batch recipe after discovering stable links or endpoints:

```python
# browser_summary:
# {
#   "fetch_progress": {
#     "kind": "extracted",
#     "message": "Fetched ${$.ok_count}/${$.total} independent URLs"
#   },
#   "records": {
#     "kind": "extracted",
#     "message": "Extracted ${$.length} records from fetched pages"
#   }
# }

urls = [...]
responses = http_get_many(urls, timeout=12, max_workers=8)
ok = [r for r in responses if not isinstance(r, dict) and getattr(r, "status_code", 0) < 400]
emit_output({"total": len(responses), "ok_count": len(ok)}, label="fetch_progress")

records = []
for url, response in zip(urls, responses):
    if isinstance(response, dict) and response.get("error"):
        records.append({"url": url, "status": "error", "error": response["error"]})
        continue
    text = response.text
    records.append({"url": url, "status": response.status_code, "title": text[:200]})

emit_output(records, label="records")
```

- Extract only fields needed for the task. Do not emit full profile text, full DOM text, cookies, localStorage, or entire app caches unless you are debugging and the smaller field-level extraction failed.
- Save complete generated result files under `outputs_dir()` or relative paths in the current working directory. Files written there are collected as artifacts automatically; `copy_artifact(...)` is for files created elsewhere.
- For large structured results, write the full JSON/CSV/text to a file. If the task asks for an exact inline final format, return that content with `done(result=...)` and optionally include `result_file=path`; otherwise finish with `done(result_file=path)`.
- For loops over multiple pages/items, emit short progress every item or every 2 seconds, whichever comes first. Progress can be a short `print(...)` line or compact `emit_output(..., label="progress")`.
- For list/profile extraction, filter the candidate list before navigating when the list page already contains enough information, such as employee versus contractor. Do not visit rows that cannot affect the final answer.
- Poll for record readiness, not for nullable answer fields. If the app cache or DOM record for a person exists but `birthday`, phone, address, or another optional field is missing/null, record that value as missing and continue instead of waiting for the optional field to appear.
- For long extraction or verification loops, prefer bounded chunks with checkpoints written to files. Use one global deadline plus per-item micro timeouts; check the global deadline before every navigation, wait, and sleep. If a chunk fails with a usable-page diagnosis, shrink the next chunk and resume from the last checkpoint.

Do not call runtime-management helpers here. There is no `browser_connect`, `browser_status`, `browser_doctor`, or `browser_recover` helper in this tool. Those are intentionally only in the `browser` tool so the model can reason about browser lifecycle explicitly.
