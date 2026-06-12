<p align="center">
  <img src="static/browser-use-terminal-banner.png" alt="Browser Use Terminal" />
</p>

# Browser Use Terminal

Automate the boring stuff in the browser.

Browser Use Terminal is a Rust TUI for browser agents. It combines a new LLM harness, Browser Harness-style CDP control, real Chrome sessions, and a terminal UI you can actually steer.

```bash
curl -fsSL https://browser-use.com/terminal/install.sh | sh
browser
```

<p align="center">
  <img src="static/terminal-preview.png" alt="Browser Use Terminal preview" />
</p>

## What It Does

- runs browser tasks from a terminal UI
- works with your logged-in Chrome when the task needs real account state
- supports headless Chromium and Browser Use cloud for clean or remote runs
- lets you watch, steer, stop, retry, and resume tasks
- keeps local history, screenshots, artifacts, and follow-ups
- uses a new LLM harness built to be 2x cheaper and 2x faster than Browser Harness

## How It Works

Browser Use Terminal is a browser-first LLM harness: Rust owns the agent loop and durable state, while the browser runtime gives the model direct CDP control over Chrome.

```text
you
 │
 ▼
browser terminal
 │
 ├─ custom Ratatui UI      watch · steer · stop · resume
 ├─ Rust LLM harness       tools · subagents · compaction · cancellation
 ├─ SQLite event log       history · screenshots · artifacts · traces
 └─ CDP browser runtime    profiles · doctor · recovery · ownership
      │
      ▼
 real Chrome  |  headless Chromium  |  Browser Use cloud
```

- The model gets raw browser capability: CDP, page JavaScript, screenshots, files, and helper code.
- The terminal stays controllable: history, artifacts, follow-ups, browser recovery, and a custom Ratatui renderer for live work.

## Try It

```text
Get my San Francisco parking permit.
```

```text
Give this employee admin permission in Azure.
```

```text
Find the cancellation policy for my current hotel reservation.
```

## Setup

Launch the app:

```bash
browser
```

Use slash commands inside the TUI:

```text
/auth      sign in
/model     choose a model
/browser   choose local, headless, or cloud browser
/update    update the app
```

Useful shell commands:

```bash
browser auth status
browser config show
browser diagnostics
```

## Use It From Claude Code, Codex, or OpenCode

Browser Use Terminal plugs into any coding assistant that can run shell commands, browser-harness style: a skill teaches the assistant the CLI, and the CLI hands it the whole browser runtime.

```bash
browser-use-terminal skill install        # registers the skill for detected assistants
```

Then ask your assistant to browse:

```bash
browser-use-terminal browser exec <<'PY'
new_tab("https://example.com")
wait_for_load()
print(capture_screenshot())
PY
```

Screenshots are saved as files and the path is printed, so assistants view them with their native file-reading tools (Claude Code `Read`, Codex `view_image`, OpenCode `read`). The browser persists between calls. See `docs/assistant-plugins.md`.

## Development

```bash
cargo fmt --check
cargo test
uv run --with pytest python -m pytest -q
scripts/verify-terminal-ui.sh
```

Terminal UI changes must pass the full verification script. It runs Rust tests, Python tests, deterministic Ratatui dumps, and a real tmux smoke test.

### Agent Tools

Release archives include a managed ripgrep binary at `bin/agent-tools/rg`.
Agent shell commands prepend Browser Use Terminal's temporary tool shim and
managed tools directory to `PATH`, so `rg`, `rg --version`, and `rg --files`
work without depending on the user's shell dotfiles or DotSlash. Source
checkout entry points and local dev wrappers install the same managed ripgrep
under `target/debug/agent-tools/rg`; run
`scripts/install-agent-ripgrep.sh target/debug/agent-tools` to refresh it
manually.

### Telemetry

You can disable (100% completely anonymous) telemetry with  `BUT_TELEMETRY=0`.

## Docs

- `docs/terminal-ui-product-ux.md`
- `docs/terminal-ui-testing.md`
- `docs/terminal-renderer-architecture.md`
- `docs/assistant-plugins.md`

## License

MIT
