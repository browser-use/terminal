# Browser Use Terminal — agent setup

You are (probably) a coding agent reading this because a user pasted
`https://browser-use.com/skill` into the chat. Follow this page top to bottom.
It installs Browser Use Terminal, registers its skill with you, connects a
browser, and verifies everything — then you can browse the web for your user.
Every step is idempotent; re-run safely. Tell the user what you're doing as
you go, and stop to ask only where this page says to.

What you get: a `browser-use-terminal` CLI you drive from shell commands —
run Python against a real browser (navigate, click, type, screenshot, extract).
You are the agent; you drive the browser.

## 1. Preflight

```bash
uname -s                 # Darwin or Linux. Windows: use WSL; native Windows is not supported yet.
command -v curl || command -v wget
command -v tar
python3 --version        # need 3.9+
```

If `python3` is missing: macOS → `xcode-select --install` (or `brew install python3`);
Debian/Ubuntu → `sudo apt-get install -y python3`; Fedora → `sudo dnf install -y python3`.
If you cannot install it, ask the user to.

## 2. Install Browser Use Terminal

```bash
curl -fsSL https://browser-use.com/terminal/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"   # the installer edits the shell profile, but YOUR current shell needs this
browser-use-terminal --version         # verify
```

Notes: no sudo needed; binaries land in `~/.local/bin`
(`browser`, `browser-use`, `browser-use-terminal`, `but`); state lives in
`~/.browser-use-terminal`. Already installed? The same command updates it.
Add the `export PATH` line to every later shell command in this session, or
rely on the profile in new shells.

## 3. Register the skill with yourself

```bash
browser-use-terminal skill install
```

This writes the skill (full usage instructions for you) into every detected
assistant home: `~/.claude/skills/`, `~/.codex/skills/`,
`~/.config/opencode/skills/`, `~/.agents/skills/`. Claude Code, Codex,
OpenCode, and anything reading those conventions will auto-discover it in new
sessions.

If you are a different agent (none of those directories exist, or you know
you don't read them): run `browser-use-terminal skill show` and persist its
output wherever you read durable instructions (e.g. your `AGENTS.md`, rules
file, or memory). If you can't persist anything, just read it now — this
session can proceed either way.

## 4. Connect a browser (pick one)

**Default — zero user interaction (recommended to start):**

```bash
browser-use-terminal browser preference use managed-headless
```

This uses a disposable headless Chromium that persists across your commands.
It requires a Chromium-family binary on the machine (Chrome, Chromium, or a
Playwright Chromium). Check: `ls "/Applications/Google Chrome.app" 2>/dev/null || command -v google-chrome chromium chromium-browser`.
If none exists: ask the user to install Chrome (or set `CHROME_PATH=/path/to/chrome`).

**The user's real, logged-in Chrome** (for tasks needing their accounts):

```bash
browser-use-terminal browser preference use local
browser-use-terminal browser connect local
```

One-time user action required: they must open `chrome://inspect/#remote-debugging`
in Chrome and tick "Allow remote debugging for this browser instance".
`browser-use-terminal browser local setup` walks them through it. Relay any
`needs-user-action` JSON (`user_prompt` field) to the user verbatim and wait.

**Cloud browser** (headless server / clean remote IP; needs an API key from
https://cloud.browser-use.com):

```bash
export BROWSER_USE_API_KEY=bu_...
browser-use-terminal browser preference use cloud
```

Cloud browsers bill until stopped (`browser-use-terminal browser recover stop-owned-remote`).

## 5. Verify end to end

```bash
browser-use-terminal browser exec <<'PY'
new_tab("https://example.com")
wait_for_load()
print(page_info()["title"])
print(capture_screenshot())
PY
```

Expected: it prints `Example Domain` and `Screenshot saved to <absolute path>`.
Now view that screenshot with your own file-reading tool to confirm you can
see pages: Claude Code → `Read` the path; Codex → `view_image` with the path;
OpenCode → `read` the path; Gemini CLI → `read_file`; otherwise use whatever
tool you have that displays a local image. If your model can't view images,
skip this — you'll work from text state (`page_info()`, `js(...)`) instead.

If this step worked, setup is complete. Tell the user you're ready to browse.

## 6. Using it (crash course)

Full instructions are in the skill you installed (or `browser-use-terminal skill show`).
The essentials:

- Run Python against the live browser with heredocs. Helpers are pre-imported:
  `new_tab(url)`, `goto_url(url)`, `page_info()`, `click_at_xy(x, y)`,
  `type_text(text)`, `press_key("Enter")`, `fill_input(sel, text)`, `scroll()`,
  `wait_for_load()`, `wait_for_element(sel)`, `capture_screenshot()`, `js(expr)`,
  `cdp(method, **params)`, `http_get(url)`.
- The browser persists between commands; Python variables do not.
- Screenshot-first workflow: screenshot → read coordinates off the image →
  `click_at_xy` (CSS pixels — divide image coordinates by
  `js("window.devicePixelRatio")`) → screenshot again to verify.
- In the user's real Chrome, open work in `new_tab(...)`, never `goto_url` over
  their active tab.
- Auth walls: stop and ask the user. Never type credentials read off a screenshot.
- Parallel workstreams: add `--session <name>` to `browser` commands.
- Done for the day: `browser-use-terminal browser recover stop-owned-browser`
  (managed) or `... stop-owned-remote` (cloud).

## Troubleshooting

- `command not found` → re-run `export PATH="$HOME/.local/bin:$PATH"`.
- `browser is not connected` → `browser-use-terminal browser connect` (uses the
  remembered preference) or re-run step 4.
- Anything returning `status: "needs-user-action"` → show its `user_prompt` to
  the user exactly, wait, then retry.
- Diagnostics: `browser-use-terminal browser doctor` and
  `browser-use-terminal browser status --json`.
- A background daemon holds the browser connection between your commands
  (auto-started). If it misbehaves: `browser-use-terminal browser daemon status`,
  `... daemon logs`, `... daemon stop` (next command restarts it and reattaches).
- Slow first command in each shell: the launcher checks for updates; set
  `BUT_AUTO_UPDATE=0` to skip.

Full documentation: https://docs.browser-use.com/open-source/browser-use-terminal
Source & issues: https://github.com/browser-use/terminal
