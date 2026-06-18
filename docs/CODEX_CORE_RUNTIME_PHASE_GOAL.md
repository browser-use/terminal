# Codex-Core Runtime Phase Goal

## Goal

Complete the Codex-core integration phase: make `browser-use-agent` emit a
Codex-compatible event/runtime contract, adapt the existing browser-use TUI to
render that contract without visual or workflow regressions, preserve
browser-use-native browser interaction, make OpenRouter the primary provider
path, and prove the result with live CLI plus real PTY TUI E2E verification and
final Codex-auth remote-browser evals.

## Non-Negotiables

- The terminal UI must keep the current `origin/main` look, layout, workflows,
  keyboard behavior, browser UX, and transcript polish.
- The runtime contract should move toward Codex, not toward a growing pile of
  browser-use-specific core event shims.
- Browser interaction remains browser-use-native and must keep the current main
  behavior.
- OpenRouter should be treated as the primary provider path for model execution,
  with Codex/OpenAI/Anthropic remaining selectable where supported.
- Final scoring evals are an explicit exception to the OpenRouter-primary rule:
  they must use Codex auth and Browser Use cloud/remote browser mode only.
- Live behavior matters more than static parity. Tool calls, browser actions,
  stream output, final answers, and TUI rendering must be proven end to end.

## Phases

### Phase 1: Event Contract

Define and implement the durable Codex-shaped event contract the agent emits:
`session.input`, `model.turn.request`, `model.stream_delta`, `model.thinking_delta`,
`model.tool_call`, `tool.started`, `tool.output`, `tool.failed`,
`tool.finished`, `token_count`, `session.done`, and `session.failed`.

The TUI/protocol layer must accept this contract directly, with compatibility for
existing browser-use session events where needed.

### Phase 2: TUI Adapter

Keep the renderer visually unchanged. Add or refactor the transcript/protocol
adapter so Codex-shaped events reduce into the same transcript nodes and
workbench state that the current UI expects.

Browser-use product extensions remain supported as first-class events:
`browser.page`, `browser.state`, `browser.live_url`, `tool.image`,
`artifact.created`, and browser-script summaries/artifacts.

### Phase 3: Tool Runtime Parity

Align model-visible tools and durable tool events with Codex where possible:
`exec_command`, `write_stdin`, `apply_patch`, `view_image`, `update_plan`,
`request_user_input`, `tool_search`, goals, subagents, MCP, and permission flows.

Keep browser-use-native tools for browser and Python, but make their emitted
events render and replay cleanly through the same event contract.

### Phase 4: Browser Preservation

Wire browser actions through the new runtime without losing current-main browser
behavior: cloud/local mode, browser page/state/live URL events, script summaries,
screenshots/images, artifacts, background observe/cancel flows, and visible TUI
activity.

### Phase 5: Provider Path

Make OpenRouter the primary provider path for normal runs. Verify tool calling,
streaming, usage accounting, model selection, provider config, and any supported
provider add-ons. Keep Codex/OpenAI/Anthropic paths selectable and honest about
unsupported features.

### Phase 6: Live Proof

Prove the integration with live runs:

- CLI run with streamed output and final `session.done`.
- Python tool call with visible `tool.started` and `tool.output`.
- Browser/cloud run with visible browser events and tool output.
- OpenRouter run with tool calling.
- Codex-auth run with tool calling.
- Real `but` TUI run in a PTY using the live engine.
- Required terminal verification via `scripts/verify-terminal-ui.sh`.

### Phase 7: Evals And Performance Report

Only after the runtime/TUI proof passes, run browser-agent evals against the
finished branch and report real performance, not just runner completion.

Required end-of-phase eval:

- `/home/exedev/datasets/real_v17_short.json`

Recommended eval procedure:

- Use a fresh `/tmp/but-...` state root for every run.
- Use only the remote Browser Use cloud browser path: pass `--browser-mode cloud`,
  set cloud browser environment defaults, and verify no local Chrome/Chromium or
  local CDP endpoint is used.
- Use Codex auth for the model path. Do not substitute OpenRouter/OpenAI API-key
  auth for the final scoring eval unless the goal is explicitly changed.
- Run with `--concurrency 25`.
- Capture the dataset manifest, session event logs, tool/browser artifacts, and
  screenshots.
- Judge outputs strictly from artifacts and final answers. A completed run only
  means the agent emitted a final answer; it is not proof of correctness.
- Report pass/fail counts, per-task verdicts, common failure classes, and the
  exact code/runtime causes for failures where identifiable.
- If the eval runner supports only named datasets, add or document the mapping
  from `/home/exedev/datasets/real_v17_short.json` to the dataset-run command.
- If the CLI still lacks a Codex-backed dataset command, adding one is part of
  this phase. The current `dataset-run-openai`/`dataset-run-openrouter` commands
  are not an acceptable replacement for a final Codex-auth eval.

Command shape for the required final eval:

```bash
STAMP="$(date -u +%Y%m%d-%H%M%S)"
ROOT="/tmp/but-decodex-real-v17-short-${STAMP}"
RUN_ID="real-v17-short-decodex-codex-cloud25-${STAMP}"
mkdir -p "$ROOT"

if [ -f .env ]; then set -a; . ./.env; set +a; fi
unset OPENAI_API_KEY LLM_BROWSER_OPENAI_API_KEY
unset ANTHROPIC_API_KEY LLM_BROWSER_ANTHROPIC_API_KEY
unset OPENROUTER_API_KEY LLM_BROWSER_OPENAI_COMPAT_API_KEY
unset BU_CDP_URL BU_CDP_WS BU_BROWSER_ID

export LLM_BROWSER_BROWSER_MODE=cloud
export LLM_BROWSER_AUTO_CHROME=0
export LLM_BROWSER_OPEN_CLOUD_LIVE_VIEW=0

stdbuf -oL -eL ./target/debug/browser-use-terminal \
  --state-dir "$ROOT/state" \
  dataset-run-codex /home/exedev/datasets/real_v17_short.json \
  --all \
  --model gpt-5.1-codex \
  --max-turns 10000 \
  --python-timeout-seconds 180 \
  --max-attempts 1 \
  --concurrency 25 \
  --browser-mode cloud \
  --run-id "$RUN_ID" 2>&1 | tee "$ROOT/run.log"
```

## Definition Of Done

- `browser-use-agent` emits Codex-compatible runtime events directly.
- The TUI renders those events with the same appearance and workflows as
  current `origin/main`.
- Browser-use-specific browser events remain supported as product extensions.
- OpenRouter is the primary, tested provider path.
- Final evals use Codex auth, Browser Use cloud/remote browser only, and
  `--concurrency 25`.
- Python/browser/tool calls persist durable visible events.
- Final answers persist as TUI-visible completion events.
- Legacy sessions still render, or a deliberate compatibility/migration path is
  documented.
- `cargo fmt --check`, `cargo test`, Python tests, deterministic TUI dumps, real
  terminal smoke, and `scripts/verify-terminal-ui.sh` pass before completion is
  claimed.
- `/home/exedev/datasets/real_v17_short.json` is run at the end and scored from
  artifacts, with a written performance report and proof that every task used
  the remote browser path.
