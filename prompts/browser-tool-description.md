Browser runtime control tool.

This tool is the browser control plane. It manages which browser is connected, who owns it, how CDP is attached, what recovery is safe, and what the current runtime knows. It does not click, type, scrape, screenshot, run page JavaScript, or inspect pixels. Use `browser_script` for page interaction.

The input is a single CLI-like command string. The leading word `browser` is optional. See the full command reference under "Commands:" below.

Mental model:

- `browser` owns runtime/control/debug; `browser_script` owns page interaction/data extraction.
- Rust holds the CDP websocket, current target id, current session id, ownership, and connection generation. Python in `browser_script` is fresh per call; variables do not persist.
- Nothing reloads, relaunches, closes, or switches tabs silently. If IDs may change, this tool reports that and you choose the next action.
- `browser status --json` may include `last_issue`, a compact diagnosis from the most recent failure; check its `next_step`, `browser_usable`, and `page_usable` before deciding to reconnect. It also lists active `browser_script` runs — use `action="observe"` to listen to them; use `browser script cancel <run_id>` only for cleanup or explicit cancellation.

Preferences:

- `preference use local|cloud|managed-headless|managed-headed` changes what plain `browser connect` means.
- `profile suggest --domain <regex>` lists remembered/local profiles (Local Chrome mode) and cloud profiles whose cookie domains match the regex (Cloud mode). If a site likely needs login and no profile is remembered, run it before connecting; in cloud mode pick a profile whose cookie domains fit the login domain, `profile remember --mode cloud --profile <profile-id>`, then `browser connect`. Do not guess friendly cloud profile names like `Work`.
- Do not silently attach to a different local profile when a profile is remembered.
- `domain skills --domain <domain>` lists matching browser-harness domain skill files; use `--include-content` to read the playbook before navigation.
- Tool commands returned in `next_step` are internal actions for you to run. Never tell the user to run `browser ...` commands manually.

Local real browser:

- `browser connect local` attaches to a local Chromium-family browser exposing CDP, only after the user enables remote debugging.
- Do not guess a browser family flag. The tool auto-detects Chrome, Chrome Canary, Chromium, Edge, Brave, Arc, Dia, Comet, and common forks through DevToolsActivePort.
- One candidate connects automatically; with multiple, ask the user which, then `browser connect local --candidate <id>`.
- If Chrome blocks with permission evidence such as 403 and `remote_debugging_enabled` is true, the checkbox is already enabled. Do not open the checkbox page. If the popup is not visible and `profile_recovery_command` is present, run it to open/focus the saved profile window, then ask the user to click Allow.
- If `state: "cdp-disabled"`, Chrome is open but the remote debugging checkbox is off. Call `browser local setup`, tell the user to enable the checkbox, then reconnect.
- If the port is closed or `DevToolsActivePort` is stale, Chrome is not exposing CDP. Do not tell the user remote debugging is disabled. If `profile_recovery_command` is present, run it then retry `browser connect local`; otherwise ask which local profile/browser to use.
- Do not launch the user's real default Chrome profile with remote-debugging flags. Real logged-in profiles are attached while already open.

Local profiles:

- `local profiles --json` (built into Rust, no external CLI) scans Chromium-family profile folders on disk. Use it when the user asks which local profiles exist or which likely contains a login. Profiles have stable ids like `google-chrome:Default`; quote ids/names with spaces, e.g. `local profiles inspect 'google-chrome:Profile 2' --domains-only`.
- `local profiles inspect <id-or-name> --domains-only` copies the profile into a temp profile, starts it with CDP, and returns only cookie domain/count/expiry metadata. Raw cookie values are never returned by default; inspection is for choosing the right profile, not dumping secrets.

Managed browser:

- `browser connect managed` starts a Rust-owned browser with a temp profile by default. `--headless`/`--headed` (default headless); `--profile <path>` only for an explicit non-default automation profile. Rust owns it and may stop/restart it; it is not the user's real logged-in Chrome.

Remote browsers:

- `browser connect remote-cdp --url <http-url>` or `--ws <ws-url>` attaches to an external DevTools HTTP endpoint or CDP websocket.
- `browser remote start ...` creates a Browser Use cloud browser and connects to it (start and connect; do not copy the returned CDP URL into another command).
- For login-sensitive cloud work, prefer `browser connect` after storing a cloud profile preference, or pass `--profile-id <uuid>` explicitly. If `--profile-name` fails, do not continue in a clean cloud browser; list with `remote profiles --json` and choose by ID/cookie domains.
- `remote stop` only stops a Browser Use cloud browser created by this runtime. `remote profiles --json` lists cloud profiles without raw cookie values.

Doctor and recovery:

- `browser doctor [--json]` is read-only: it checks runtime state, local candidates, profile discovery, API key, websocket/target health, and safe next steps, but never fixes state itself — if a fix is available it prints an explicit command.
- `recover reconnect-websocket` reconnects the CDP websocket to the same endpoint (never reloads the page). `recover reattach-same-target` attaches a fresh session to the same target id (reports available targets, never silently switches, if it is gone). `recover restart-runtime` resets the Rust connection holder and reconnects to the same endpoint (does not kill Chrome). `recover restart-owned-browser` restarts only Rust-owned managed browsers; `recover stop-owned-remote` stops only Rust-owned cloud browsers.

Commands:

```text
browser help
browser status --json
browser doctor
browser doctor --json

browser preference --json
browser preference use local|cloud|managed-headless|managed-headed
browser profile suggest --domain <regex> --json
browser profile use <profile-id>
browser profile remember --domain <domain> --profile <profile-id> [--mode local|cloud|managed-headless]
browser profile forget --domain <domain>
browser domain skills --domain <domain> [--include-content] --json

browser connect
browser connect local
browser connect local --candidate <id>
browser connect managed [--headless|--headed] [--profile temp|<path>] [--arg <chrome-arg>...]
browser connect remote-cdp --url <http-url>
browser connect remote-cdp --ws <ws-url>

browser local list --json
browser local open --profile <profile-id>
browser local setup [--profile <profile-id>]
browser local profiles --json
browser local profiles inspect <profile-id-or-name> --domains-only

browser remote start [--profile-id <uuid>|--profile-name <name>] [--timeout <minutes>] [--proxy-country <iso2|none>]
browser remote stop
browser remote status --json
browser remote live-url
browser remote profiles --json

browser recover reconnect-websocket
browser recover reattach-same-target
browser recover restart-runtime
browser recover restart-owned-browser
browser recover stop-owned-remote

browser script runs --json
browser script cancel <run_id>

browser runtime logs
browser runtime ownership --json
browser runtime cleanup-stale
```

Use `browser status --json` before recovery when the situation is unclear. Use `browser runtime ownership --json` before stopping anything. External user Chrome is never killed or relaunched by this tool.
