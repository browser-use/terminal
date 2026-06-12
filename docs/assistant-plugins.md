# Using Browser Use Terminal from coding assistants

Browser Use Terminal plugs into any coding assistant or agent that can run shell commands — Claude Code, Codex, OpenCode, OpenClaw, Cursor CLI, and friends. The model is similar to [browser-use/browser-harness](https://github.com/browser-use/browser-harness): a skill file teaches the assistant the CLI, and the CLI gives it the full browser runtime (connect/recovery control plane, Python page helpers, screenshots-as-files).

Fastest setup: paste `https://browser-use.com/skill` into your assistant, and it provides instructions on how to install, register the skill, connect a browser, and verify. Full docs: <https://docs.browser-use.com/open-source/browser-use-terminal>.

The assistant is the agent. The skill teaches it to drive the browser directly:

```bash
browser-use-terminal browser exec <<'PY'
new_tab("https://example.com")
wait_for_load()
print(capture_screenshot())
PY
```

(The built-in agent — the TUI and `browser-use-terminal run` — remains a human-facing surface and is not part of the skill.)

## Install

1. Install Browser Use Terminal so `browser-use-terminal` is on `$PATH`:

   ```bash
   curl -fsSL https://browser-use.com/terminal/install.sh | sh
   ```

2. Register the skill. With no argument this installs for every assistant whose home directory exists (`~/.claude`, `~/.codex`, `~/.config/opencode`, `~/.agents`):

   ```bash
   browser-use-terminal skill install
   # or one of:
   browser-use-terminal skill install claude
   browser-use-terminal skill install codex
   browser-use-terminal skill install opencode
   browser-use-terminal skill install agents
   ```

   `browser-use-terminal skill paths` prints where each assistant's copy lands; `skill show` prints the markdown. Re-run `skill install` after updating the CLI to refresh the copies.

   Notes:
   - OpenCode also discovers Claude-compatible skill paths (`~/.claude/skills/...`), so the `claude` install covers both.
   - Gemini CLI has no skills directory; paste the output of `browser-use-terminal skill show` into `~/.gemini/GEMINI.md` (or a project `GEMINI.md`) instead.

3. Pick a browser (one-time). The CLI auto-connects per the remembered preference on every call:

   ```bash
   browser-use-terminal browser preference use managed-headless   # zero-setup disposable Chromium (default-quality choice)
   browser-use-terminal browser preference use local              # your real, logged-in Chrome
   browser-use-terminal browser preference use cloud              # Browser Use cloud (needs BROWSER_USE_API_KEY)
   ```

   For `local`, Chrome needs its one-time remote-debugging opt-in: open `chrome://inspect/#remote-debugging` and tick "Allow remote debugging for this browser instance" (`browser-use-terminal browser local setup` walks the user through it).

## How it stays stateful across bash calls

Each CLI invocation is a thin client over a long-lived daemon. The first `browser` command auto-spawns one daemon per state dir. It hosts the browser session registries — and therefore the live CDP websocket — across invocations. This is what keeps Chrome's per-connection debugging-permission popup to a single Allow in local mode, exactly like the TUI. The CLI and daemon speak line-delimited JSON over a unix socket (`$TMPDIR/but-browser-<statehash>.sock`, mode 0600); a version handshake restarts stale daemons after CLI updates, and if the daemon can't start the CLI falls back to in-process execution with a warning. Operate it with `browser-use-terminal browser daemon status|stop|logs`.

The browsers themselves also survive daemon restarts:

- **local** — your Chrome keeps running; the daemon holds one authorized CDP connection to it.
- **managed** — Chromium launches with a stable per-session profile and a marker file (`<state-dir>/external-browser/managed/<session>/`), outlives the daemon, and is reattached. Stop it with `browser-use-terminal browser recover stop-owned-browser`.
- **cloud** — the created browser's id/CDP URL is recorded (`<state-dir>/external-browser/cloud/<session>.json`) and reattached until it stops or times out. Stop it (and billing) with `browser-use-terminal browser recover stop-owned-remote`.

Page/tab state therefore persists between `browser exec` calls; Python variables do not (each exec is a fresh interpreter). `--session <name>` gives parallel workstreams isolated artifact dirs, event logs, and managed browsers (requests share one daemon and run serially).

Everything an assistant does is recorded in the same SQLite event log the TUI uses — inspect with `browser-use-terminal events browser-cli-<session>` or `browser-use-terminal sessions list`.

## How assistants see screenshots

Bash output is text-only, so images travel as files: `capture_screenshot()` saves a PNG (downscaled to ≤1800 px per side for this surface) and the CLI prints `Screenshot saved to <absolute path>`. The skill then tells each assistant to use its native image-reading tool:

| Assistant | Tool | Notes |
|---|---|---|
| Claude Code | `Read` on the path | reads PNG/JPG natively |
| Codex CLI | `view_image` with `{"path": ...}` | resizes >2048 px itself; enabled by default |
| OpenCode | `read` on the path | image support since Oct 2025; needs a vision model |
| Gemini CLI | `read_file` on the path | returns inline image data |
| Cursor CLI | reference the path | the agent reads image files automatically |

Non-vision models skip screenshots and work from text state (`page_info()`, `js(...)`, `wait_for_element(...)`) — the skill spells out this fallback.

## Credentials, navigation policy, and safety

The external CLI surface enforces the same policies as the in-app agent on every call:

- Secrets/TOTP stored via `browser-use-terminal secrets ...` are available to scripts only as placeholders (`type_text("<secret>name</secret>")`); raw values never reach the assistant.
- The domain allow/deny policy (`browser-use-terminal domains ...`) guards `Page.navigate` at the Rust layer.
- Blocked states surface as `needs-user-action` JSON with a `user_prompt` the assistant is instructed to relay verbatim instead of guessing.

## Files

- `SKILL.md` (repo root) — the assistant-facing skill, embedded into the binary at build time and written out by `skill install`.
- `crates/browser-use-cli` — `browser` / `skill` subcommands.
- `crates/browser-use-agent/src/tools/handlers/browser.rs` — `run_external_browser_command` / `run_external_browser_script`, the blocking entry points that reuse the in-session preference resolution, auto-connect, security policy, and event persistence.
- `crates/browser-use-browser` — persistent managed/cloud browser reattach (`BROWSER_USE_TERMINAL_PERSIST_BROWSERS`, `BU_EXTERNAL_BROWSER_STATE_DIR`).
