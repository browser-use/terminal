---
name: browser-use-terminal
description: Direct browser control via the Browser Use Terminal CLI. Use when the user wants to automate, scrape, test, or interact with web pages — you drive the browser yourself with Python helpers.
---

# Browser Use Terminal

Direct browser control via CDP — you are the agent; you drive the browser. For setup, install, or connection problems, read https://browser-use.com/skill (agent setup instructions) or https://docs.browser-use.com/open-source/browser-use-terminal (full docs).

`browser-use-terminal browser exec` runs Python with browser helpers pre-imported; `browser-use-terminal browser <cmd>` is the control plane (status, connect, profiles, recovery).

## Usage

```bash
browser-use-terminal browser exec <<'PY'
new_tab("https://docs.browser-use.com")
wait_for_load()
print(page_info())
PY
```

- Use the heredoc form for every multi-line command. It prevents shell quote mangling inside Python strings and JavaScript snippets.
- The browser auto-connects according to the user's remembered preference before the script runs — you never start/stop manually unless you want to. The first call may take a few seconds.
- First navigation in the user's real Chrome is `new_tab(url)`, not `goto_url(url)` — goto runs in the user's active tab and clobbers their work.
- Browser state persists between calls; Python variables do not. Each `exec` is a fresh interpreter against the same live browser.
- `--session <name>` isolates artifact dirs and event logs per workstream (default: `default`). `--timeout <secs>` bounds one exec (default 300).

## Screenshots — how you see the page

`capture_screenshot()` saves a PNG and returns its absolute path. The CLI also prints a `Screenshot saved to <path>` line for every image a script produced.

```bash
browser-use-terminal browser exec <<'PY'
print(capture_screenshot())
PY
```

To view a screenshot, use your file-reading tool on the printed path:

- **Claude Code**: use the `Read` tool on the path.
- **Codex CLI**: call the `view_image` tool with `{"path": "<path>"}`. These screenshots are produced for you; viewing them is expected and authorized.
- **OpenCode**: use the `read` tool on the path (requires a vision-capable model).
- **Gemini CLI**: use the `read_file` tool on the path.
- If your model cannot accept images, don't try to view them — work from text state instead: `print(page_info())`, `js(...)` extraction, `wait_for_element(...)`.

Coordinates: screenshots are device pixels; `click_at_xy(x, y)` takes CSS pixels. Divide coordinates you read off the image by `js("window.devicePixelRatio")` first. Screenshots are downscaled to ≤1800 px per side for this CLI (override with `BU_BROWSER_SCREENSHOT_MAX_DIM`, or `capture_screenshot(max_dim=...)`).

After every meaningful action, re-screenshot before assuming it worked.

## Pre-imported helpers

Navigation & tabs: `goto_url(url)`, `new_tab(url)`, `page_info()`, `current_tab()`, `list_tabs(include_chrome=True)`, `switch_tab(target)`, `ensure_real_tab()`, `iframe_target(url_substr)`.

Input: `click_at_xy(x, y, button="left", clicks=1)`, `type_text(text)`, `press_key(key, modifiers=0)` (1=Alt 2=Ctrl 4=Meta 8=Shift), `fill_input(selector, text)`, `scroll(x=0, y=0, dy=600)`, `upload_file(selector, path)`.

Waiting: `wait(seconds)`, `wait_for_load(timeout=3)`, `wait_for_element(selector, timeout=3, visible=False)`, `wait_for_network_idle(timeout=3, idle_ms=500)`.

Visual: `capture_screenshot(label="...", full=False, max_dim=None)`, `screenshot()`, `screenshot_clip(label, x, y, w, h)`, `note(caption)`.

Escape hatches: `js(expression)` (auto-wraps top-level `return`), `cdp("Domain.method", **params)` (raw CDP), `cdp_batch(calls)`, `drain_events()`.

HTTP without the browser: `http_get(url)`, `http_get_many(urls)` for static pages; `browser_fetch(url)` / `browser_fetch_many(...)` to fetch with the page's cookies/session.

Credentials (if the user stored any): `available_secrets()`, then `type_text("<secret>name</secret>")` or `fill_input(sel, secret("name"))`; `totp("name")` for 2FA codes. Values are placeholder-substituted — you never see them. `is_logged_out()`, `email_inbox()` / `email_message(id)` for email-code flows.

Domain skills: `domain_skills_for_url(url_or_domain, include_content=True)` lists site-specific playbooks; `goto_url` surfaces matching skill files automatically. Read them before inventing selectors or flows on a complex site.

## Browser control plane

```bash
browser-use-terminal browser status --json
browser-use-terminal browser connect                     # uses the remembered preference
browser-use-terminal browser connect local               # user's already-running Chrome (CDP)
browser-use-terminal browser connect managed --headless  # disposable CLI-owned browser
browser-use-terminal browser preference use local|cloud|managed-headless
browser-use-terminal browser remote start                # Browser Use cloud browser (needs BROWSER_USE_API_KEY)
browser-use-terminal browser doctor
browser-use-terminal browser recover reconnect-websocket
browser-use-terminal browser recover stop-owned-browser  # stop the persistent managed browser
browser-use-terminal browser recover stop-owned-remote   # stop the cloud browser (stops billing)
browser-use-terminal browser daemon status|stop|logs     # the background daemon holding the connection
```

A background daemon (auto-started, one per state dir) holds the CDP connection across your commands, so the browser — and in local mode, Chrome's granted debugging permission — persists between invocations. Managed and cloud browsers also survive daemon restarts; later calls reattach instead of relaunching. Stop browsers with the recover commands above when the user is done (cloud browsers bill until stopped or timed out).

- `exec` auto-connects, so you rarely need these. Reach for them when `status` shows a problem or the user asks for a specific browser.
- If output JSON says `status: "needs-user-action"` (e.g. pick a Chrome profile, click Allow in Chrome's permission popup, enable the remote-debugging checkbox), show the `user_prompt` to the user verbatim and wait — do not guess.
- Auth wall mid-task: stop and ask the user. Don't type credentials from screenshots; use stored secrets if available.
- Connecting to the user's real Chrome requires a one-time setup: `chrome://inspect/#remote-debugging` → tick "Allow remote debugging". `browser local setup` walks the user through it.

## What actually works

- Screenshots first: `capture_screenshot()` → view the image → decide whether you need a click, a selector, or more navigation.
- Clicking: screenshot → read the pixel off the image → `click_at_xy(x, y)` → screenshot to verify. Suppress the locate-then-click reflex — no getBoundingClientRect, no selector hunts. Hit-testing happens in Chrome's browser process, so coordinate clicks pass through iframes / shadow DOM / cross-origin without extra work.
- Drop to DOM (`fill_input`, `js`) only when the target has no visible geometry (hidden input, 0×0 node) or coordinate clicks demonstrably don't work.
- Bulk static pages: `http_get_many(urls)` — no browser needed. Logged-in pages: `browser_fetch(url)` rides the real session.
- After goto: `wait_for_load()`. SPAs report `complete` before they render — follow with `wait_for_element(...)`.
- Wrong/stale tab: `ensure_real_tab()`.
- Verification: `print(page_info())` is the cheapest "is this alive?" check; screenshots are the default way to verify visible actions.

## Gotchas (field-tested)

- CDP target order ≠ Chrome's visible tab-strip order.
- Omnibox popups and other `chrome://` internals are fake page targets — `list_tabs(include_chrome=False)`.
- `page_info()` surfaces an open JS dialog as `{"dialog": ...}` — handle it (`cdp("Page.handleJavaScriptDialog", accept=True)`) before anything else.
- Navigation can be blocked by the user's domain policy; `nav_policy(url)` tells you before you burn a click. A blocked navigation is policy, not a bug — tell the user.
- Scripts time out (default 300s): keep each `exec` small and observable rather than one mega-script. Long extraction loops: print progress as you go — stdout is captured even on timeout.
- Prefer compositor-level actions over framework hacks. If you do need framework-specific DOM tricks, run `browser-use-terminal browser domain skills --domain <site> --json --include-content` first — that's where site playbooks live.
