#!/usr/bin/env bash
set -euo pipefail

is_google_chrome_wrapper() {
  [[ -f "$1" ]] && grep -q 'Google Chrome.app' "$1"
}

find_allowed_google_chrome() {
  local candidate
  for candidate in \
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
    "/opt/google/chrome/google-chrome"
  do
    if [[ -x "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done

  for candidate in google-chrome google-chrome-stable; do
    if command -v "$candidate" >/dev/null 2>&1; then
      command -v "$candidate"
      return 0
    fi
  done

  return 1
}

if [[ -n "${CHROME_PATH:-}" ]]; then
  chrome="$CHROME_PATH"
elif compgen -G "$HOME/Library/Caches/ms-playwright/chromium-*/chrome-mac*/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing" >/dev/null; then
  chrome="$(find "$HOME/Library/Caches/ms-playwright" -path '*/chromium-*/chrome-mac*/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing' -type f | sort -r | head -1)"
elif [[ -x /opt/homebrew/Caskroom/chromium/latest/chrome-mac/Chromium.app/Contents/MacOS/Chromium ]]; then
  chrome="/opt/homebrew/Caskroom/chromium/latest/chrome-mac/Chromium.app/Contents/MacOS/Chromium"
elif [[ -x /Applications/Chromium.app/Contents/MacOS/Chromium ]]; then
  chrome="/Applications/Chromium.app/Contents/MacOS/Chromium"
elif command -v chromium >/dev/null 2>&1 && ! is_google_chrome_wrapper "$(command -v chromium)"; then
  chrome="$(command -v chromium)"
elif [[ -x /opt/homebrew/bin/chromium ]] && ! is_google_chrome_wrapper /opt/homebrew/bin/chromium; then
  chrome="/opt/homebrew/bin/chromium"
elif [[ "${LLM_BROWSER_ALLOW_GOOGLE_CHROME:-}" == "1" ]] && chrome="$(find_allowed_google_chrome)"; then
  :
else
  echo "Chromium not found; install Chromium, set CHROME_PATH, or set LLM_BROWSER_ALLOW_GOOGLE_CHROME=1" >&2
  exit 1
fi
port="${LLM_BROWSER_LIVE_CHROME_PORT:-9341}"
state_dir="${LLM_BROWSER_LIVE_STATE_DIR:-/tmp/but-live-browser-boundary}"
profile="$(mktemp -d /tmp/but-chrome-profile.XXXXXX)"
log="${LLM_BROWSER_LIVE_CHROME_LOG:-/tmp/but-chrome-live-smoke.log}"

"$chrome" \
  --headless=new \
  --remote-debugging-address=127.0.0.1 \
  --remote-debugging-port="$port" \
  --user-data-dir="$profile" \
  --no-first-run \
  --no-default-browser-check \
  about:blank >"$log" 2>&1 &
chrome_pid=$!

cleanup() {
  kill "$chrome_pid" >/dev/null 2>&1 || true
  wait "$chrome_pid" >/dev/null 2>&1 || true
  rm -rf "$profile" >/dev/null 2>&1 || true
}
trap cleanup EXIT

for _ in $(seq 1 80); do
  if python3 - <<PY >/dev/null 2>&1
import json, urllib.request
json.loads(urllib.request.urlopen("http://127.0.0.1:$port/json/version", timeout=0.5).read())
PY
  then
    break
  fi
  sleep 0.25
done

python3 - <<PY >/dev/null
import json, urllib.request
json.loads(urllib.request.urlopen("http://127.0.0.1:$port/json/version", timeout=2).read())
PY

rm -rf "$state_dir"

download_task="$(
  BU_CDP_URL="http://127.0.0.1:$port" \
    uv run browser-use-terminal --state-dir "$state_dir" start "dedicated chrome download regression"
)"

BU_CDP_URL="http://127.0.0.1:$port" \
  uv run browser-use-terminal --state-dir "$state_dir" browser-script "$download_task" '
from pathlib import Path
import time

root = Path(artifact_root())
download_dir = root / "downloads"
download_dir.mkdir(parents=True, exist_ok=True)
cdp("Browser.setDownloadBehavior", behavior="allow", downloadPath=str(download_dir), eventsEnabled=True)
goto_url("data:text/html,<html><title>download smoke</title><body><a id=dl download=report.csv href=\"data:text/csv,alpha%2Cbeta%250A1%2C2%250A\">download</a></body></html>")
wait_for_load(5)
js("document.querySelector(\"#dl\").click()")

deadline = time.time() + 10
downloaded = None
while time.time() < deadline:
    matches = list(download_dir.glob("report*.csv"))
    if matches and not any(path.name.endswith(".crdownload") for path in download_dir.iterdir()):
        downloaded = matches[0]
        break
    time.sleep(0.2)

if downloaded is None:
    raise RuntimeError(f"download did not finish; files={list(path.name for path in download_dir.iterdir())}")

copy_artifact(downloaded, kind="file")
result = {"download": str(downloaded), "text": downloaded.read_text()}
'

stale_task="$(
  BU_CDP_URL="http://127.0.0.1:$port" \
    uv run browser-use-terminal --state-dir "$state_dir" start "dedicated chrome stale recovery regression"
)"

BU_CDP_URL="http://127.0.0.1:$port" \
  uv run browser-use-terminal --state-dir "$state_dir" browser-script "$stale_task" '
goto_url("data:text/html,<title>stale session smoke patched</title><h1>ok</h1>")
wait_for_load(5)
before = current_tab()
_send_meta("set_session", session_id="stale-session-for-regression", target_id=before["targetId"])
title = js("document.title")
after = current_tab()

if before["targetId"] != after["targetId"]:
    raise RuntimeError(f"target changed across stale-session recovery: before={before} after={after}")

result = {"before": before, "after": after, "title": title}
'

printf 'download task: %s\nstale recovery task: %s\nstate dir: %s\n' "$download_task" "$stale_task" "$state_dir"
