Browser runtime control tool.

This tool is the browser control plane. It manages which browser is connected, who owns it, how CDP is attached, what recovery is safe, and what the current runtime knows. It does not click, type, scrape, screenshot, run page JavaScript, or inspect pixels. Use `browser_script` for page interaction.

The input is a single CLI-like command string. You may include the leading word `browser`, but it is optional:

```text
browser status --json
browser preference --json
browser preference use local
browser profile suggest --domain example.com --json
browser profile remember --domain example.com --profile google-chrome:Profile 2
browser domain skills --domain example.com --json
browser connect
browser connect local
browser local list --json
browser local open --profile google-chrome:Profile 2
browser local setup
browser local setup --profile google-chrome:Profile 2
browser connect managed --headed
browser remote profiles --json
browser remote start --profile-id <cloud-profile-id>
browser recover reconnect-websocket
browser script runs --json
browser script cancel <run_id>
```

Mental model:

- `browser` owns runtime/control/debug.
- `browser_script` owns page interaction/data extraction.
- Rust holds the CDP websocket, current target id, current session id, ownership, and connection generation.
- Python in `browser_script` is fresh per call; Python variables do not persist.
- Nothing reloads, relaunches, closes, or switches tabs silently. If IDs may change, this tool reports that and you choose the next action.
- `browser status --json` may include `last_issue`, a compact diagnosis from the most recent browser/browser_script failure. Use its `next_step`, `browser_usable`, and `page_usable` fields before deciding to reconnect.
- `browser status --json` also lists active `browser_script` runs. Use the `browser_script` tool with `action="observe"` to listen to them; use `browser script cancel <run_id>` only for cleanup or explicit cancellation.

Preferences:

- `browser preference --json` shows the remembered browser mode/profile preferences.
- `browser preference use local|cloud|managed-headless|managed-headed` changes what plain `browser connect` means.
- `browser profile suggest --domain <regex> --json` lists remembered and local profile options for a site in Local Chrome mode, and cloud profiles whose cookie domains match the regex in Browser Use Cloud mode.
- `browser profile remember --domain <domain> --profile <profile-id> [--mode local|cloud]` stores the profile to use next time for that domain.
- `browser domain skills --domain <domain> --json` lists matching browser-harness domain skill files. Use `--include-content` when you need to read the playbook before navigation.
- If a site likely needs login and no profile is remembered, run `browser profile suggest --domain <regex> --json` before connecting. In cloud mode, choose a profile whose matching cookie domains fit the target login domain, run `browser profile remember --mode cloud --profile <profile-id>`, then `browser connect`. Do not guess friendly cloud profile names like `Work`.
- Do not silently attach to a different local profile when a profile is remembered.
- Tool commands returned in `next_step` are internal actions for you to run. Never tell the user to run `browser ...` commands manually.

Local real browser:

- `browser connect local` checks for a local Chromium-family browser exposing CDP and attaches only after the user enables remote debugging.
- Do not guess a browser family flag. The tool auto-detects Chrome, Chrome Canary, Chromium, Edge, Brave, Arc, Dia, Comet, and common forks through DevToolsActivePort.
- If one candidate exists, it connects. If multiple candidates exist, ask the user which candidate to use, then run `browser connect local --candidate <id>`.
- If Chrome blocks the connection with permission evidence such as 403 and `remote_debugging_enabled` is true, the checkbox is already enabled. Do not open the checkbox page. If the popup is not visible and `profile_recovery_command` is present, run it to open/focus the saved profile window, then ask the user to click Allow in Chrome's permission popup.
- If the tool reports `state: "cdp-disabled"`, Chrome is open but not exposing CDP because the remote debugging checkbox is off. Call `browser local setup`; tell the user to enable the checkbox in Chrome, then reconnect.
- If the port is closed or `DevToolsActivePort` is stale, Chrome is not exposing CDP right now. Do not tell the user remote debugging is disabled. If `profile_recovery_command` is present, run it to open the saved profile window, then retry `browser connect local`. Otherwise ask which local profile/browser to use.
- Do not launch the user's real default Chrome profile with remote-debugging flags. Real logged-in profiles are attached while already open.

Local profiles:

- `browser local profiles --json` is built into Rust. It scans Chromium-family profile folders on disk and does not require any external CLI.
- Use local profile listing when the user asks which local browser profiles exist or which profile likely contains a login.
- Profiles have stable ids like `google-chrome:Default`; use that id for inspection when possible.
- If a profile id or name contains spaces, quote it like `browser local profiles inspect 'google-chrome:Profile 2' --domains-only`.
- `browser local profiles inspect <profile-id-or-name> --domains-only` copies the selected profile into a temporary browser profile, starts that temporary copy with CDP, and returns only cookie domain/count/expiry metadata.
- Raw cookie values are never returned by default. Profile inspection is for choosing the right profile, not for dumping secrets.

Managed browser:

- `browser connect managed` starts a Rust-owned browser with a temp profile by default.
- Use `--headless` or `--headed`; default is headless.
- Use `--profile <path>` only for an explicit non-default automation profile.
- Rust may stop/restart this browser because Rust owns it. It is not the user's real logged-in Chrome.

Remote browsers:

- `browser connect remote-cdp --url <http-url>` attaches to an external DevTools HTTP endpoint.
- `browser connect remote-cdp --ws <ws-url>` attaches to an external CDP websocket.
- `browser remote start ...` creates a Browser Use cloud browser and connects to it. Remote start means start and connect; do not copy the returned CDP URL into another command.
- For login-sensitive cloud work, prefer `browser connect` after storing a cloud profile preference, or pass `--profile-id <uuid>` explicitly. If `--profile-name` fails, do not continue in a clean cloud browser; list profiles with `browser remote profiles --json` and choose by ID/cookie domains.
- `browser remote stop` only stops a Browser Use cloud browser created by this runtime.
- `browser remote profiles --json` lists cloud profiles without raw cookie values.

Doctor:

- `browser doctor` and `browser doctor --json` are read-only.
- Doctor checks runtime state, local browser candidates, Rust local profile discovery, API key, CDP websocket health, current target health, and safe next steps.
- Doctor never fixes state by itself. If a fix is available it prints an explicit command.

Recovery:

- `browser recover reconnect-websocket`: reconnects the CDP websocket to the same endpoint. It never reloads the page.
- `browser recover reattach-same-target`: attaches a fresh CDP session to the same target id. If the target is gone, it reports available targets and does not silently switch.
- `browser recover restart-runtime`: resets the Rust connection holder and reconnects to the same endpoint. It does not kill Chrome.
- `browser recover restart-owned-browser`: restarts only Rust-owned managed browsers.
- `browser recover stop-owned-remote`: stops only Rust-owned Browser Use cloud browsers.

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
