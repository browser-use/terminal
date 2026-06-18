//! Simple Codex-style harness contract.
//!
//! This module intentionally does not implement an agent brain. It owns the
//! boring-but-load-bearing pieces that made the Codex + browser-harness eval arm
//! reproducible: per-session filesystem layout, packaged browser skill assets,
//! run-local browser-harness environment, append-only event mirroring, and final
//! answer capture from `session.done` only.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use std::collections::hash_map::DefaultHasher;

#[cfg(unix)]
use std::os::unix::net::UnixStream;

use anyhow::{bail, Context, Result};
use browser_use_protocol::{session_result_from_events, EventRecord, SessionMeta};
use browser_use_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config_overrides::ProviderRunConfig;

/// Source of truth for the packaged assistant-facing browser skill.
pub const PACKAGED_BROWSER_SKILL_MD: &str =
    include_str!("../../../prompts/browser-harness-skill.md");

pub const SIMPLE_HARNESS_PREPARED_EVENT: &str = "harness.prepared";
pub const SIMPLE_HARNESS_MIRRORED_EVENT: &str = "harness.mirrored";
pub const SIMPLE_HARNESS_CLEANED_EVENT: &str = "harness.cleaned";
pub const SIMPLE_HARNESS_VERSION: &str = env!("CARGO_PKG_VERSION");

static WORKER_CHILDREN: OnceLock<Mutex<HashMap<String, Child>>> = OnceLock::new();

const BROWSER_SKILL_RELATIVE_PATH: &[&str] = &["skills", "browser", "SKILL.md"];
const PATH_ENV: &str = "PATH";
const FORCE_CLOUD_MARKER: &str = ".browser-use-force-cloud";
const ARTIFACT_AUDIT_COMMAND_NAME: &str = "artifact-audit";
const BROWSER_HARNESS_WORKER_COMMAND_NAME: &str = "browser-harness-worker";
const BROWSER_HARNESS_WORKER_CLIENT_COMMAND_NAME: &str = "browser-harness-worker-client";
const AGENT_HELPERS_FILE_NAME: &str = "agent_helpers.py";
const WORKER_SOCKET_ENV: &str = "BU_HARNESS_WORKER_SOCKET";
const PRODUCT_BROWSER_MODE_ENV: &str = "BUT_BROWSER_MODE";
const PRODUCT_BROWSER_PROFILE_ID_ENV: &str = "BUT_BROWSER_PROFILE_ID";
const PRODUCT_BROWSER_PROFILE_LABEL_ENV: &str = "BUT_BROWSER_PROFILE_LABEL";
const PRODUCT_BROWSER_LOCAL_BROWSER_ENV: &str = "BUT_BROWSER_LOCAL_BROWSER";
const PRODUCT_STATE_DIR_ENV: &str = "BUT_STATE_DIR";
const PRODUCT_CLI_BIN_ENV: &str = "BUT_BROWSER_USE_TERMINAL_BIN";
const PRODUCT_SECRET_META_ENV: &str = "BU_BROWSER_SECRET_META";
const CLOUD_AUTOSPAWN_PROFILE_ID_ENV: &str = "BU_AUTOSPAWN_PROFILE_ID";
const CLOUD_AUTOSPAWN_PROFILE_NAME_ENV: &str = "BU_AUTOSPAWN_PROFILE_NAME";
#[cfg(test)]
const WORKER_ACTIVE_ENV: &str = "BU_HARNESS_WORKER_ACTIVE";
const BROWSER_COMMAND_SHIM: &str = r#"#!/usr/bin/env bash
self_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
harness="$self_dir/browser-harness"
if [ "$#" -eq 0 ] && [ ! -t 0 ]; then
  exec "$harness"
fi
if [ "$#" -eq 0 ] || [ "$1" = "connect" ] || [ "$1" = "status" ]; then
  exec "$harness" <<'PY'
ensure_real_tab()
print(page_info())
PY
fi
exec "$harness" "$@"
"#;
const BROWSER_HARNESS_COMMAND_SHIM: &str = r#"#!/usr/bin/env bash
set -e

self_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
real_browser_harness=""
IFS=':' read -r -a path_entries <<< "${PATH:-}"
for entry in "${path_entries[@]}"; do
  [ -n "$entry" ] || continue
  [ "$entry" = "$self_dir" ] && continue
  if [ -x "$entry/browser-harness" ]; then
    real_browser_harness="$entry/browser-harness"
    break
  fi
done
if [ -z "$real_browser_harness" ]; then
  echo "browser-harness wrapper: real browser-harness not found on PATH" >&2
  exit 127
fi

should_bootstrap_cloud=0
force_cloud=0
force_cloud_marker="$self_dir/../../.browser-use-force-cloud"
case "${1:-}" in
  -h|--help|--version|--doctor|doctor|--update|--reload)
    should_bootstrap_cloud=0
    ;;
  *)
    if [ -f "$force_cloud_marker" ] \
      || [ "${BU_FORCE_CLOUD:-}" = "1" ] \
      || [ "${LLM_BROWSER_BROWSER_MODE:-}" = "cloud" ]; then
      force_cloud=1
      if [ -z "${BROWSER_USE_API_KEY:-}" ]; then
        echo "browser-harness wrapper: cloud mode requires BROWSER_USE_API_KEY" >&2
        exit 64
      fi
      unset BU_CDP_URL BU_CDP_WS BU_BROWSER_ID
      should_bootstrap_cloud=1
    fi
    ;;
esac

if [ "$should_bootstrap_cloud" = "1" ]; then
  real_python=""
  first_line="$(head -n 1 "$real_browser_harness" 2>/dev/null || true)"
  case "$first_line" in
    '#!'*) real_python="${first_line#\#!}" ;;
  esac
  if [ -z "$real_python" ] || [ ! -x "$real_python" ]; then
    real_python="python3"
  fi
  PYTHONPATH="${BROWSER_HARNESS_SRC:-}:${PYTHONPATH:-}" "$real_python" <<'PY'
import os
from browser_harness import _ipc as ipc
from browser_harness.admin import NAME, daemon_alive, restart_daemon, start_remote_daemon

def daemon_is_remote(name):
    if not daemon_alive(name):
        return False
    try:
        lines = ipc.log_path(name).read_text(errors="ignore").splitlines()
    except Exception:
        return False
    for line in reversed(lines):
        if "listening on " in line and " remote=" in line:
            return "remote=local" not in line
    return False

def daemon_has_cdp(name):
    if not daemon_alive(name):
        return False
    try:
        c, token = ipc.connect(name, timeout=3.0)
        try:
            resp = ipc.request(c, token, {"method": "Target.getTargets", "params": {}})
        finally:
            try:
                c.close()
            except Exception:
                pass
    except Exception:
        return False
    return isinstance(resp, dict) and "result" in resp

if daemon_alive(NAME) and (not daemon_is_remote(NAME) or not daemon_has_cdp(NAME)):
    restart_daemon(NAME)

if not daemon_alive(NAME):
    timeout = int(os.environ.get("BH_CLOUD_TIMEOUT_MINUTES", "60"))
    kwargs = {"timeout": timeout}
    profile_id = os.environ.get("BU_AUTOSPAWN_PROFILE_ID")
    profile_name = os.environ.get("BU_AUTOSPAWN_PROFILE_NAME")
    if profile_id:
        kwargs["profileId"] = profile_id
    elif profile_name:
        kwargs["profileName"] = profile_name
    start_remote_daemon(NAME, **kwargs)
PY
fi

if [ -n "${BROWSER_HARNESS_SRC:-}" ]; then
  export PYTHONPATH="${BROWSER_HARNESS_SRC}${PYTHONPATH:+:$PYTHONPATH}"
fi

worker_client="$self_dir/browser-harness-worker-client"
if [ -n "${BU_HARNESS_WORKER_SOCKET:-}" ] && [ -x "$worker_client" ]; then
  REAL_BROWSER_HARNESS="$real_browser_harness" exec "$worker_client" "$@"
fi

exec "$real_browser_harness" "$@"
"#;
const BROWSER_HARNESS_WORKER_CLIENT_SHIM: &str = r#"#!/usr/bin/env python3
import base64
import json
import os
import socket
import subprocess
import sys


def run_direct(real, argv, stdin_bytes):
    proc = subprocess.Popen(
        [real, *argv],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=os.environ.copy(),
    )
    stdout, stderr = proc.communicate(stdin_bytes)
    sys.stdout.buffer.write(stdout)
    sys.stderr.buffer.write(stderr)
    return proc.returncode


def main():
    real = os.environ.get("REAL_BROWSER_HARNESS")
    if not real:
        print("browser-harness worker client: REAL_BROWSER_HARNESS missing", file=sys.stderr)
        return 127
    argv = sys.argv[1:]
    stdin_bytes = sys.stdin.buffer.read()
    socket_path = os.environ.get("BU_HARNESS_WORKER_SOCKET")
    if not socket_path or not hasattr(socket, "AF_UNIX"):
        return run_direct(real, argv, stdin_bytes)
    req = {
        "real_browser_harness": real,
        "argv": argv,
        "stdin_b64": base64.b64encode(stdin_bytes).decode("ascii"),
        "env": dict(os.environ),
    }
    try:
        conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        conn.settimeout(float(os.environ.get("BU_HARNESS_WORKER_CLIENT_TIMEOUT", "300")))
        conn.connect(socket_path)
        with conn:
            conn.sendall((json.dumps(req) + "\n").encode("utf-8"))
            chunks = []
            while True:
                chunk = conn.recv(1 << 16)
                if not chunk:
                    break
                chunks.append(chunk)
                if chunk.endswith(b"\n"):
                    break
        resp = json.loads(b"".join(chunks).decode("utf-8") or "{}")
    except Exception:
        return run_direct(real, argv, stdin_bytes)
    sys.stdout.buffer.write(base64.b64decode(resp.get("stdout_b64") or ""))
    sys.stderr.buffer.write(base64.b64decode(resp.get("stderr_b64") or ""))
    return int(resp.get("exit_code", 1))


if __name__ == "__main__":
    raise SystemExit(main())
"#;
const BROWSER_HARNESS_WORKER_PY: &str = r#"#!/usr/bin/env python3
import argparse
import base64
import json
import os
import signal
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

EVENTS_PATH = None
EVENTS_LOCK = threading.Lock()
SECRET_REDACTIONS = []
SECRET_REDACTIONS_LOCK = threading.Lock()


def read_request(conn):
    data = b""
    while not data.endswith(b"\n"):
        chunk = conn.recv(1 << 16)
        if not chunk:
            break
        data += chunk
    return json.loads(data.decode("utf-8") or "{}")


def write_response(conn, response):
    conn.sendall((json.dumps(response) + "\n").encode("utf-8"))


def write_event(event, **payload):
    if EVENTS_PATH is None:
        return
    try:
        record = {
            "event": event,
            "pid": os.getpid(),
            "ts": time.time(),
            **payload,
        }
        EVENTS_PATH.parent.mkdir(parents=True, exist_ok=True)
        with EVENTS_LOCK:
            with EVENTS_PATH.open("a", encoding="utf-8") as fh:
                fh.write(json.dumps(record, sort_keys=True) + "\n")
    except Exception:
        pass


def remember_secret(value, label):
    if not value:
        return
    with SECRET_REDACTIONS_LOCK:
        item = (str(value), str(label or "secret"))
        if item not in SECRET_REDACTIONS:
            SECRET_REDACTIONS.append(item)


def redact_bytes(data):
    if not data:
        return data
    with SECRET_REDACTIONS_LOCK:
        redactions = list(SECRET_REDACTIONS)
    if not redactions:
        return data
    redactions.sort(key=lambda item: len(item[0]), reverse=True)
    out = data
    for value, label in redactions:
        if len(value) < 4:
            continue
        out = out.replace(
            value.encode("utf-8", "ignore"),
            f"<secret>{label}</secret>".encode("utf-8"),
        )
    return out


def resolve_secret(req):
    state_dir = req.get("state_dir") or os.environ.get("BUT_STATE_DIR")
    if not state_dir:
        return {"ok": False, "error": "BUT_STATE_DIR missing"}
    domain = str(req.get("domain") or "").strip()
    name = str(req.get("name") or "").strip()
    if not domain or not name:
        return {"ok": False, "error": "secret.resolve requires domain and name"}
    cli = req.get("cli") or os.environ.get("BUT_BROWSER_USE_TERMINAL_BIN") or "browser-use-terminal"
    env = dict(os.environ)
    env["BU_HARNESS_WORKER_ACTIVE"] = "1"
    proc = subprocess.run(
        [
            cli,
            "--state-dir",
            state_dir,
            "secrets",
            "harness-secret",
            "--domain",
            domain,
            "--name",
            name,
        ],
        text=True,
        capture_output=True,
        env=env,
    )
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or "").strip()
        return {"ok": False, "error": detail or f"{cli} exited with status {proc.returncode}"}
    try:
        payload = json.loads(proc.stdout.strip() or "{}")
    except Exception as exc:
        return {"ok": False, "error": f"invalid secret bridge response: {exc}"}
    value = payload.get("value")
    label = payload.get("label") or name
    if value is None:
        return {"ok": False, "error": f"secret {name!r} returned no value"}
    remember_secret(value, label)
    write_event("secret.resolved", domain=domain, name=name, value_bytes=len(str(value)))
    return {"ok": True, "value": value}


def run_browser_harness(req):
    real = req["real_browser_harness"]
    argv = req.get("argv") or []
    stdin_bytes = base64.b64decode(req.get("stdin_b64") or "")
    env = dict(req.get("env") or os.environ)
    env["BU_HARNESS_WORKER_ACTIVE"] = "1"
    write_event(
        "request.started",
        argv=argv,
        real_browser_harness=real,
        stdin_bytes=len(stdin_bytes),
    )
    proc = subprocess.Popen(
        [real, *argv],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    stdout, stderr = proc.communicate(stdin_bytes)
    stdout = redact_bytes(stdout)
    stderr = redact_bytes(stderr)
    write_event(
        "request.finished",
        argv=argv,
        exit_code=proc.returncode,
        stderr_bytes=len(stderr),
        stdout_bytes=len(stdout),
    )
    return {
        "exit_code": proc.returncode,
        "stdout_b64": base64.b64encode(stdout).decode("ascii"),
        "stderr_b64": base64.b64encode(stderr).decode("ascii"),
    }


def main():
    global EVENTS_PATH
    parser = argparse.ArgumentParser()
    parser.add_argument("--socket", required=True)
    parser.add_argument("--pid", required=True)
    parser.add_argument("--events", required=True)
    args = parser.parse_args()
    sock_path = Path(args.socket)
    pid_path = Path(args.pid)
    EVENTS_PATH = Path(args.events)
    sock_path.parent.mkdir(parents=True, exist_ok=True)
    try:
        sock_path.unlink()
    except FileNotFoundError:
        pass
    pid_path.write_text(str(os.getpid()))
    write_event("worker.started", pid_path=str(pid_path), socket_path=str(sock_path))
    stop = threading.Event()

    def handle_signal(signum, frame):
        write_event("worker.signal", signum=signum)
        stop.set()

    signal.signal(signal.SIGTERM, handle_signal)
    signal.signal(signal.SIGINT, handle_signal)

    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(sock_path))
    os.chmod(sock_path, 0o600)
    server.listen(16)
    server.settimeout(0.25)
    try:
        while not stop.is_set():
            try:
                conn, _ = server.accept()
            except socket.timeout:
                continue
            with conn:
                try:
                    req = read_request(conn)
                    if req.get("meta") == "ping":
                        write_event("worker.ping")
                        write_response(conn, {"ok": True, "pid": os.getpid()})
                    elif req.get("meta") == "shutdown":
                        write_event("worker.shutdown_requested")
                        write_response(conn, {"ok": True, "pid": os.getpid()})
                        stop.set()
                    elif req.get("meta") == "secret.resolve":
                        write_response(conn, resolve_secret(req))
                    else:
                        write_response(conn, run_browser_harness(req))
                except BaseException as exc:
                    message = f"{type(exc).__name__}: {exc}\n"
                    write_event("request.error", error=message.strip())
                    write_response(conn, {
                        "exit_code": 1,
                        "stdout_b64": "",
                        "stderr_b64": base64.b64encode(message.encode("utf-8", "replace")).decode("ascii"),
                    })
    finally:
        write_event("worker.stopped")
        server.close()
        try:
            sock_path.unlink()
        except FileNotFoundError:
            pass
        try:
            pid_path.unlink()
        except FileNotFoundError:
            pass


if __name__ == "__main__":
    if not hasattr(socket, "AF_UNIX"):
        print("browser-harness worker requires Unix sockets", file=sys.stderr)
        raise SystemExit(2)
    main()
"#;
const AGENT_HELPERS_PY: &str = r#"# Generated by browser-use-terminal simple harness.
# Loaded by browser-harness from BH_AGENT_WORKSPACE/agent_helpers.py.
import json
import os
import re
import socket
import subprocess
from datetime import datetime, timezone
from urllib.parse import urlparse

from browser_harness import helpers as _bh

_ORIGINAL_NEW_TAB = _bh.new_tab
_ORIGINAL_GOTO_URL = getattr(_bh, "goto_url", None)
_ORIGINAL_HTTP_GET = _bh.http_get
_ORIGINAL_TYPE_TEXT = _bh.type_text
_ORIGINAL_FILL_INPUT = _bh.fill_input


def _json_list_env(name):
    raw = os.environ.get(name, "").strip()
    if not raw:
        return []
    try:
        value = json.loads(raw)
    except Exception:
        return []
    if not isinstance(value, list):
        return []
    return [str(item).strip().lower() for item in value if str(item).strip()]


def _json_object_env(name):
    raw = os.environ.get(name, "").strip()
    if not raw:
        return {}
    try:
        value = json.loads(raw)
    except Exception:
        return {}
    return value if isinstance(value, dict) else {}


def nav_policy():
    """Return the product /domains policy visible to this harness session."""
    return {
        "allowed_domains": _json_list_env("BU_BROWSER_ALLOWED_DOMAINS"),
        "denied_domains": _json_list_env("BU_BROWSER_PROHIBITED_DOMAINS"),
    }


def _host_for_url(url):
    parsed = urlparse(str(url))
    if parsed.scheme and parsed.scheme not in ("http", "https"):
        return None
    return (parsed.hostname or "").strip(".").lower() or None


def _matches_domain(pattern, host):
    pattern = (pattern or "").strip(".").lower()
    host = (host or "").strip(".").lower()
    if not pattern or not host:
        return False
    if pattern.startswith("*."):
        pattern = pattern[2:]
    return host == pattern or host.endswith("." + pattern)


def _blocked_reason(url):
    host = _host_for_url(url)
    if not host:
        return None
    policy = nav_policy()
    denied = policy["denied_domains"]
    allowed = policy["allowed_domains"]
    if any(_matches_domain(pattern, host) for pattern in denied):
        return f"{host} is denied by /domains"
    if allowed and not any(_matches_domain(pattern, host) for pattern in allowed):
        return f"{host} is outside the /domains allow-list"
    return None


def _assert_url_allowed(url):
    reason = _blocked_reason(url)
    if reason:
        raise RuntimeError(
            f"Navigation blocked by browser-use-terminal domain policy: {reason}. "
            "Use nav_policy() to inspect the current policy."
        )


def new_tab(url, *args, **kwargs):
    _assert_url_allowed(url)
    return _ORIGINAL_NEW_TAB(url, *args, **kwargs)


if _ORIGINAL_GOTO_URL is not None:
    def goto_url(url, *args, **kwargs):
        _assert_url_allowed(url)
        return _ORIGINAL_GOTO_URL(url, *args, **kwargs)


def http_get(url, *args, **kwargs):
    _assert_url_allowed(url)
    return _ORIGINAL_HTTP_GET(url, *args, **kwargs)


def _product_cli():
    return os.environ.get("BUT_BROWSER_USE_TERMINAL_BIN") or "browser-use-terminal"


def _worker_request(payload):
    socket_path = os.environ.get("BU_HARNESS_WORKER_SOCKET")
    if not socket_path:
        raise RuntimeError("browser-use-terminal secret bridge is unavailable.")
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.settimeout(float(os.environ.get("BU_HARNESS_WORKER_CLIENT_TIMEOUT", "300")))
    try:
        conn.connect(socket_path)
        conn.sendall((json.dumps(payload) + "\n").encode("utf-8"))
        chunks = []
        while True:
            chunk = conn.recv(1 << 16)
            if not chunk:
                break
            chunks.append(chunk)
            if chunk.endswith(b"\n"):
                break
    finally:
        conn.close()
    return json.loads(b"".join(chunks).decode("utf-8") or "{}")


_SECRET_TAG_RE = re.compile(r"<secret>(.*?)</secret>")


def _secret_meta():
    return _json_object_env("BU_BROWSER_SECRET_META")


def _secret_current_domain():
    try:
        url = _bh.current_tab().get("url", "") or ""
    except Exception:
        url = ""
    return (urlparse(url).hostname or "").strip(".").lower()


def _secret_domain_matches(domain, pattern):
    pattern = (pattern or "").strip().lstrip("*").lstrip(".").lower()
    domain = (domain or "").strip().strip(".").lower()
    return bool(pattern and domain and (domain == pattern or domain.endswith("." + pattern)))


def _applicable_secret_meta():
    domain = _secret_current_domain()
    out = {}
    for pattern, names in _secret_meta().items():
        if not isinstance(names, dict) or not _secret_domain_matches(domain, pattern):
            continue
        for name, info in names.items():
            is_totp = bool(info.get("totp")) if isinstance(info, dict) else False
            out[str(name)] = (is_totp, str(pattern))
    return out


def available_secrets():
    """Return saved credential placeholder names for the current page domain."""
    return sorted(_applicable_secret_meta().keys())


def _resolve_secret_value(name, applicable=None):
    applicable = _applicable_secret_meta() if applicable is None else applicable
    if name not in applicable:
        where = _secret_current_domain() or "this page"
        raise RuntimeError(f"no secret named {name!r} is configured for {where}.")
    _is_totp, pattern = applicable[name]
    resp = _worker_request({
        "meta": "secret.resolve",
        "state_dir": os.environ.get("BUT_STATE_DIR"),
        "cli": _product_cli(),
        "domain": pattern,
        "name": name,
    })
    if not resp.get("ok"):
        raise RuntimeError(resp.get("error") or f"secret {name!r} could not be read")
    return str(resp.get("value") or "")


def _substitute_secrets(text):
    if not text:
        return text
    applicable = _applicable_secret_meta()
    if not applicable:
        return text
    text = str(text)
    if "<secret>" in text:
        def _replace(match):
            name = match.group(1)
            return _resolve_secret_value(name, applicable) if name in applicable else match.group(0)
        return _SECRET_TAG_RE.sub(_replace, text)
    if text.strip() in applicable:
        return _resolve_secret_value(text.strip(), applicable)
    return text


def secret(name):
    """Return a saved credential placeholder, not the real value."""
    name = str(name)
    if name not in _applicable_secret_meta():
        where = _secret_current_domain() or "this page"
        raise RuntimeError(f"no secret named {name!r} is configured for {where}.")
    return f"<secret>{name}</secret>"


def totp(name):
    """Return a saved TOTP-code placeholder, not the real code."""
    name = str(name)
    applicable = _applicable_secret_meta()
    if name not in applicable or not applicable[name][0]:
        where = _secret_current_domain() or "this page"
        raise RuntimeError(f"no TOTP secret named {name!r} is configured for {where}.")
    return f"<secret>{name}</secret>"


def _product_cli_json(*args):
    state_dir = os.environ.get("BUT_STATE_DIR")
    if not state_dir:
        raise RuntimeError("browser-use-terminal state dir is unavailable for email helpers.")
    cmd = [_product_cli(), "--state-dir", state_dir, *[str(arg) for arg in args]]
    proc = subprocess.run(cmd, text=True, capture_output=True)
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or "").strip()
        raise RuntimeError(detail or f"{cmd[0]} exited with status {proc.returncode}")
    text = proc.stdout.strip()
    if not text:
        return None
    try:
        return json.loads(text)
    except Exception as exc:
        raise RuntimeError(f"email helper expected JSON from browser-use-terminal: {exc}: {text[:500]}")


def current_datetime():
    """Return the current UTC time in model-friendly forms."""
    now = datetime.now(timezone.utc)
    return {
        "utc": now.isoformat(timespec="milliseconds").replace("+00:00", "Z"),
        "unix": now.timestamp(),
    }


def email_address():
    """Return the agent's disposable inbox address."""
    state_dir = os.environ.get("BUT_STATE_DIR")
    if not state_dir:
        raise RuntimeError("No email inbox is configured for this harness session.")
    cmd = [_product_cli(), "--state-dir", state_dir, "secrets", "email", "address"]
    proc = subprocess.run(cmd, text=True, capture_output=True)
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or "").strip()
        raise RuntimeError(detail or "email inbox is unavailable")
    address = proc.stdout.strip()
    if not address:
        raise RuntimeError("email inbox is unavailable")
    return address


def _parse_email_timestamp(value):
    if value is None or value == "":
        return None
    if isinstance(value, (int, float)):
        return datetime.fromtimestamp(float(value), tz=timezone.utc)
    text = str(value).strip()
    if not text:
        return None
    if re.fullmatch(r"\d+(\.\d+)?", text):
        number = float(text)
        if number > 10_000_000_000:
            number = number / 1000.0
        return datetime.fromtimestamp(number, tz=timezone.utc)
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def email_inbox(limit=20, sent_after=None):
    """List recent messages in the agent's inbox, newest first."""
    messages = _product_cli_json("secrets", "email", "inbox", "--limit", int(limit)) or []
    cutoff = _parse_email_timestamp(sent_after)
    if cutoff is None:
        return messages
    return [
        message
        for message in messages
        if (parsed := _parse_email_timestamp(message.get("timestamp"))) is not None and parsed > cutoff
    ]


def email_message(message_id):
    """Read one inbox message's full content by message_id."""
    message = _product_cli_json("secrets", "email", "message", str(message_id))
    if not message:
        raise RuntimeError(f"message {message_id!r} not found in inbox.")
    return message


def type_text(text):
    return _ORIGINAL_TYPE_TEXT(_substitute_secrets(str(text)))


def fill_input(selector, text, *args, **kwargs):
    return _ORIGINAL_FILL_INPUT(selector, _substitute_secrets(str(text)), *args, **kwargs)
"#;
const ARTIFACT_AUDIT_COMMAND_SHIM: &str =
    include_str!("../../../prompts/simple-harness-artifact-audit.py");
const SIMPLE_HARNESS_SYSTEM_PREAMBLE: &str = r#"You are Codex, a pragmatic coding agent running inside a terminal session.

For browser tasks, use the browser-harness skill below through shell or exec_command. Prefer retrieved data from tool output over memory. Do not fabricate page contents, prices, IDs, counts, or availability. If the task is blocked by auth, network, or missing data, say that directly. When finished, answer normally in the final assistant message."#;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimpleHarnessPaths {
    pub session_id: String,
    pub state_dir: PathBuf,
    pub cwd: PathBuf,
    pub artifact_root: PathBuf,
    pub home: PathBuf,
    pub agent_workspace: PathBuf,
    pub domain_skills_root: PathBuf,
    pub runtime_dir: PathBuf,
    pub tmp_dir: PathBuf,
    pub browser_skill_path: PathBuf,
    pub browser_command_path: PathBuf,
    pub browser_harness_command_path: PathBuf,
    pub browser_harness_worker_command_path: PathBuf,
    pub browser_harness_worker_client_command_path: PathBuf,
    pub artifact_audit_command_path: PathBuf,
    pub force_cloud_marker_path: PathBuf,
    pub worker_socket_path: PathBuf,
    pub worker_pid_path: PathBuf,
    pub worker_log_path: PathBuf,
    pub worker_events_jsonl_path: PathBuf,
    pub events_jsonl: PathBuf,
    pub final_txt: PathBuf,
}

impl SimpleHarnessPaths {
    pub fn from_session(state_dir: impl Into<PathBuf>, session: &SessionMeta) -> Self {
        let state_dir = state_dir.into();
        let artifact_root = PathBuf::from(&session.artifact_root);
        let home = artifact_root.join("home");
        let agent_workspace = home.join("agent-workspace");
        let domain_skills_root = agent_workspace.join("domain-skills");
        let runtime_dir = env::temp_dir()
            .join("browser-use-simple-harness")
            .join(format!(
                "{}-{}",
                sanitize_env_id(&session.id),
                short_hash(&state_dir.display().to_string())
            ));
        let tmp_dir = artifact_root.join("tmp");
        let browser_skill_path = BROWSER_SKILL_RELATIVE_PATH
            .iter()
            .fold(home.clone(), |path, part| path.join(part));
        let local_bin = home.join(".local").join("bin");
        let browser_command_path = local_bin.join("browser");
        let browser_harness_command_path = local_bin.join("browser-harness");
        let browser_harness_worker_command_path =
            local_bin.join(BROWSER_HARNESS_WORKER_COMMAND_NAME);
        let browser_harness_worker_client_command_path =
            local_bin.join(BROWSER_HARNESS_WORKER_CLIENT_COMMAND_NAME);
        let artifact_audit_command_path = local_bin.join(ARTIFACT_AUDIT_COMMAND_NAME);
        let force_cloud_marker_path = home.join(FORCE_CLOUD_MARKER);
        let worker_socket_path = runtime_dir.join("browser-harness-worker.sock");
        let worker_pid_path = runtime_dir.join("browser-harness-worker.pid");
        let worker_log_path = tmp_dir.join("browser-harness-worker.log");
        let worker_events_jsonl_path = tmp_dir.join("browser-harness-worker-events.jsonl");
        Self {
            session_id: session.id.clone(),
            state_dir,
            cwd: PathBuf::from(&session.cwd),
            artifact_root: artifact_root.clone(),
            home,
            agent_workspace,
            domain_skills_root,
            runtime_dir,
            tmp_dir,
            browser_skill_path,
            browser_command_path,
            browser_harness_command_path,
            browser_harness_worker_command_path,
            browser_harness_worker_client_command_path,
            artifact_audit_command_path,
            force_cloud_marker_path,
            worker_socket_path,
            worker_pid_path,
            worker_log_path,
            worker_events_jsonl_path,
            events_jsonl: artifact_root.join("events.jsonl"),
            final_txt: artifact_root.join("final.txt"),
        }
    }

    pub fn ensure_prepared(&self) -> Result<SimpleHarnessPrepared> {
        fs::create_dir_all(&self.cwd)
            .with_context(|| format!("create harness cwd {}", self.cwd.display()))?;
        fs::create_dir_all(&self.artifact_root).with_context(|| {
            format!(
                "create harness artifact root {}",
                self.artifact_root.display()
            )
        })?;
        fs::create_dir_all(&self.home)
            .with_context(|| format!("create harness home {}", self.home.display()))?;
        fs::create_dir_all(&self.agent_workspace).with_context(|| {
            format!(
                "create harness agent workspace {}",
                self.agent_workspace.display()
            )
        })?;
        fs::create_dir_all(&self.domain_skills_root).with_context(|| {
            format!(
                "create harness domain-skills root {}",
                self.domain_skills_root.display()
            )
        })?;
        fs::create_dir_all(&self.runtime_dir).with_context(|| {
            format!("create harness runtime dir {}", self.runtime_dir.display())
        })?;
        fs::create_dir_all(&self.tmp_dir)
            .with_context(|| format!("create harness tmp dir {}", self.tmp_dir.display()))?;
        if let Some(parent) = self.browser_skill_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create browser skill dir {}", parent.display()))?;
        }
        let skill_changed = write_if_changed(&self.browser_skill_path, PACKAGED_BROWSER_SKILL_MD)
            .with_context(|| {
            format!("sync browser skill {}", self.browser_skill_path.display())
        })?;
        let agent_helpers_path = self.agent_workspace.join(AGENT_HELPERS_FILE_NAME);
        let agent_helpers_changed = write_if_changed(&agent_helpers_path, AGENT_HELPERS_PY)
            .with_context(|| format!("sync agent helpers {}", agent_helpers_path.display()))?;
        let browser_command_changed =
            write_executable_if_changed(&self.browser_command_path, BROWSER_COMMAND_SHIM)
                .with_context(|| {
                    format!(
                        "sync browser command shim {}",
                        self.browser_command_path.display()
                    )
                })?;
        let browser_harness_command_changed = write_executable_if_changed(
            &self.browser_harness_command_path,
            BROWSER_HARNESS_COMMAND_SHIM,
        )
        .with_context(|| {
            format!(
                "sync browser-harness command shim {}",
                self.browser_harness_command_path.display()
            )
        })?;
        let browser_harness_worker_command_changed = write_executable_if_changed(
            &self.browser_harness_worker_command_path,
            BROWSER_HARNESS_WORKER_PY,
        )
        .with_context(|| {
            format!(
                "sync browser-harness worker {}",
                self.browser_harness_worker_command_path.display()
            )
        })?;
        let browser_harness_worker_client_command_changed = write_executable_if_changed(
            &self.browser_harness_worker_client_command_path,
            BROWSER_HARNESS_WORKER_CLIENT_SHIM,
        )
        .with_context(|| {
            format!(
                "sync browser-harness worker client {}",
                self.browser_harness_worker_client_command_path.display()
            )
        })?;
        let artifact_audit_command_changed = write_executable_if_changed(
            &self.artifact_audit_command_path,
            ARTIFACT_AUDIT_COMMAND_SHIM,
        )
        .with_context(|| {
            format!(
                "sync artifact audit command {}",
                self.artifact_audit_command_path.display()
            )
        })?;
        Ok(SimpleHarnessPrepared {
            paths: self.clone(),
            skill_changed,
            browser_command_changed: browser_command_changed
                || browser_harness_command_changed
                || browser_harness_worker_command_changed
                || browser_harness_worker_client_command_changed
                || artifact_audit_command_changed
                || agent_helpers_changed,
            worker: self.worker_state(false, false, None),
        })
    }

    /// Add harness environment to the native provider run.
    ///
    /// These vars are scoped through the run config so both the Python worker
    /// path and the shell/exec path can see the same per-session harness layout.
    pub fn apply_to_config(&self, config: &mut ProviderRunConfig) {
        for (key, value) in self.harness_env() {
            upsert_env(&mut config.options.python_env, &key, value);
        }
        let browser_mode = trimmed_option(config.options.browser_mode.as_deref());
        let browser_profile_id = trimmed_option(config.options.browser_profile_id.as_deref());
        let browser_profile_label = trimmed_option(config.options.browser_profile_label.as_deref());
        let browser_local_browser = trimmed_option(config.options.browser_local_browser.as_deref());

        if let Some(mode) = browser_mode.as_deref() {
            upsert_env(
                &mut config.options.python_env,
                PRODUCT_BROWSER_MODE_ENV,
                mode,
            );
        }
        if let Some(profile_id) = browser_profile_id.as_deref() {
            upsert_env(
                &mut config.options.python_env,
                PRODUCT_BROWSER_PROFILE_ID_ENV,
                profile_id,
            );
        }
        if let Some(profile_label) = browser_profile_label.as_deref() {
            upsert_env(
                &mut config.options.python_env,
                PRODUCT_BROWSER_PROFILE_LABEL_ENV,
                profile_label,
            );
        }
        if let Some(local_browser) = browser_local_browser.as_deref() {
            upsert_env(
                &mut config.options.python_env,
                PRODUCT_BROWSER_LOCAL_BROWSER_ENV,
                local_browser,
            );
        }

        if browser_mode.as_deref() == Some("cloud") {
            upsert_env(&mut config.options.python_env, "BU_FORCE_CLOUD", "1");
            upsert_env(
                &mut config.options.python_env,
                "LLM_BROWSER_BROWSER_MODE",
                "cloud",
            );
            upsert_env(
                &mut config.options.python_env,
                "LLM_BROWSER_AUTO_CHROME",
                "0",
            );
            upsert_env(
                &mut config.options.python_env,
                "LLM_BROWSER_OPEN_CLOUD_LIVE_VIEW",
                "0",
            );
            upsert_env(&mut config.options.python_env, "BU_CDP_URL", "");
            upsert_env(&mut config.options.python_env, "BU_CDP_WS", "");
            upsert_env(&mut config.options.python_env, "BU_BROWSER_ID", "");
            if let Some(profile_id) = browser_profile_id.as_deref() {
                upsert_env(
                    &mut config.options.python_env,
                    CLOUD_AUTOSPAWN_PROFILE_ID_ENV,
                    profile_id,
                );
            } else if let Some(profile_label) = browser_profile_label.as_deref() {
                upsert_env(
                    &mut config.options.python_env,
                    CLOUD_AUTOSPAWN_PROFILE_NAME_ENV,
                    profile_label,
                );
            }
        }
    }

    pub fn sync_force_cloud_marker(&self, force_cloud: bool) -> Result<bool> {
        if force_cloud {
            return write_if_changed(&self.force_cloud_marker_path, "1\n");
        }
        match fs::remove_file(&self.force_cloud_marker_path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err).with_context(|| {
                format!(
                    "remove force-cloud marker {}",
                    self.force_cloud_marker_path.display()
                )
            }),
        }
    }

    pub fn harness_env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            ("CODEX_HOME".to_string(), self.home.display().to_string()),
            (
                PRODUCT_STATE_DIR_ENV.to_string(),
                self.state_dir.display().to_string(),
            ),
            (
                "BROWSER_USE_HARNESS_HOME".to_string(),
                self.home.display().to_string(),
            ),
            (
                "BH_AGENT_WORKSPACE".to_string(),
                self.agent_workspace.display().to_string(),
            ),
            (
                "BH_DOMAIN_SKILLS_ROOT".to_string(),
                self.domain_skills_root.display().to_string(),
            ),
            (
                "BH_RUNTIME_DIR".to_string(),
                self.runtime_dir.display().to_string(),
            ),
            ("BH_TMP_DIR".to_string(), self.tmp_dir.display().to_string()),
            (
                WORKER_SOCKET_ENV.to_string(),
                self.worker_socket_path.display().to_string(),
            ),
            // Arm C disabled domain skills. Preserve that parity unless a caller
            // deliberately overrides it after harness preparation.
            ("BH_DOMAIN_SKILLS".to_string(), "0".to_string()),
            ("BU_AUTOSPAWN".to_string(), "1".to_string()),
            (
                "BU_NAME".to_string(),
                format!("sh{}", sanitize_env_id(&self.session_id)),
            ),
        ];
        if let Some(path) = browser_harness_path_value(&self.home) {
            env.push((PATH_ENV.to_string(), path));
        }
        if let Some(path) = browser_use_terminal_bin_value() {
            env.push((PRODUCT_CLI_BIN_ENV.to_string(), path));
        }
        copy_process_env_if_present(&mut env, "BROWSER_USE_API_KEY");
        env
    }

    fn worker_state(
        &self,
        enabled: bool,
        already_running: bool,
        pid: Option<u32>,
    ) -> SimpleHarnessWorker {
        SimpleHarnessWorker {
            enabled,
            already_running,
            pid,
            socket_path: self.worker_socket_path.clone(),
            pid_path: self.worker_pid_path.clone(),
            log_path: self.worker_log_path.clone(),
            events_jsonl_path: self.worker_events_jsonl_path.clone(),
        }
    }

    pub fn start_worker(&self) -> Result<SimpleHarnessWorker> {
        start_worker_for_paths(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimpleHarnessPrepared {
    pub paths: SimpleHarnessPaths,
    pub skill_changed: bool,
    pub browser_command_changed: bool,
    pub worker: SimpleHarnessWorker,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimpleHarnessWorker {
    pub enabled: bool,
    pub already_running: bool,
    pub pid: Option<u32>,
    pub socket_path: PathBuf,
    pub pid_path: PathBuf,
    pub log_path: PathBuf,
    pub events_jsonl_path: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimpleHarnessDomainPolicy {
    pub allowed_domains: Vec<String>,
    pub denied_domains: Vec<String>,
    pub env_applied: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimpleHarnessSecretPolicy {
    pub secret_count: usize,
    pub env_applied: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimpleHarnessMirror {
    pub events_written: usize,
    pub final_written: bool,
    pub final_bytes: usize,
    pub final_txt: PathBuf,
    pub events_jsonl: PathBuf,
}

pub fn prepare_existing_session(
    store: &Store,
    session_id: &str,
    config: &mut ProviderRunConfig,
) -> Result<SimpleHarnessPrepared> {
    let session = store
        .load_session(session_id)?
        .with_context(|| format!("unknown session id: {session_id}"))?;
    let paths = SimpleHarnessPaths::from_session(store.state_dir().to_path_buf(), &session);
    let prepared = paths.ensure_prepared()?;
    let force_cloud = config.options.browser_mode.as_deref() == Some("cloud");
    let force_cloud_marker_changed = prepared.paths.sync_force_cloud_marker(force_cloud)?;
    let domain_policy = apply_store_domain_policy_env(store, config)?;
    let secret_policy = apply_store_secret_metadata_env(store, config)?;
    let worker = prepared.paths.start_worker()?;
    prepared.paths.apply_to_config(config);
    let browser_config = simple_harness_browser_config(config);
    store.append_event(
        session_id,
        SIMPLE_HARNESS_PREPARED_EVENT,
        json!({
            "version": SIMPLE_HARNESS_VERSION,
            "cwd": prepared.paths.cwd,
            "home": prepared.paths.home,
            "artifact_root": prepared.paths.artifact_root,
            "events_jsonl": prepared.paths.events_jsonl,
            "final_txt": prepared.paths.final_txt,
            "browser_skill_path": prepared.paths.browser_skill_path,
            "browser_command_path": prepared.paths.browser_command_path,
            "browser_harness_command_path": prepared.paths.browser_harness_command_path,
            "browser_harness_worker_command_path": prepared.paths.browser_harness_worker_command_path,
            "browser_harness_worker_client_command_path": prepared.paths.browser_harness_worker_client_command_path,
            "browser_harness_worker_events_jsonl": prepared.paths.worker_events_jsonl_path,
            "artifact_audit_command_path": prepared.paths.artifact_audit_command_path,
            "worker": &worker,
            "force_cloud": force_cloud,
            "browser_config": browser_config,
            "domain_policy": domain_policy,
            "secret_policy": secret_policy,
            "force_cloud_marker_path": prepared.paths.force_cloud_marker_path,
            "force_cloud_marker_changed": force_cloud_marker_changed,
            "skill_changed": prepared.skill_changed,
            "browser_command_changed": prepared.browser_command_changed,
        }),
    )?;
    Ok(SimpleHarnessPrepared { worker, ..prepared })
}

fn apply_store_domain_policy_env(
    store: &Store,
    config: &mut ProviderRunConfig,
) -> Result<SimpleHarnessDomainPolicy> {
    let (mut allowed_domains, denied_domains) =
        crate::tools::handlers::secrets_admin::list_domains(store)?;
    if !allowed_domains.is_empty() {
        for meta in crate::tools::handlers::secrets_admin::list_secrets(store)? {
            if !allowed_domains.iter().any(|domain| domain == &meta.domain) {
                allowed_domains.push(meta.domain);
            }
            for domain in meta.allowed_domains {
                if !allowed_domains.iter().any(|existing| existing == &domain) {
                    allowed_domains.push(domain);
                }
            }
        }
        allowed_domains.sort();
    }
    let env_applied = !allowed_domains.is_empty() || !denied_domains.is_empty();
    if !allowed_domains.is_empty() {
        upsert_env(
            &mut config.options.python_env,
            "BU_BROWSER_ALLOWED_DOMAINS",
            serde_json::to_string(&allowed_domains)?,
        );
    }
    if !denied_domains.is_empty() {
        upsert_env(
            &mut config.options.python_env,
            "BU_BROWSER_PROHIBITED_DOMAINS",
            serde_json::to_string(&denied_domains)?,
        );
    }
    Ok(SimpleHarnessDomainPolicy {
        allowed_domains,
        denied_domains,
        env_applied,
    })
}

fn apply_store_secret_metadata_env(
    store: &Store,
    config: &mut ProviderRunConfig,
) -> Result<SimpleHarnessSecretPolicy> {
    let metas = crate::tools::handlers::secrets_admin::list_secrets(store)?;
    if metas.is_empty() {
        return Ok(SimpleHarnessSecretPolicy::default());
    }
    let mut blob = serde_json::Map::new();
    for meta in &metas {
        let value = json!({
            "totp": matches!(meta.kind, browser_use_secrets::SecretKind::Totp),
        });
        for domain in std::iter::once(&meta.domain).chain(meta.allowed_domains.iter()) {
            let entry = blob
                .entry(domain.clone())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if let serde_json::Value::Object(map) = entry {
                map.insert(meta.placeholder.clone(), value.clone());
            }
        }
    }
    upsert_env(
        &mut config.options.python_env,
        PRODUCT_SECRET_META_ENV,
        serde_json::to_string(&serde_json::Value::Object(blob))?,
    );
    Ok(SimpleHarnessSecretPolicy {
        secret_count: metas.len(),
        env_applied: true,
    })
}

pub fn simple_harness_system_prompt(base_override: Option<&str>) -> String {
    let preamble = base_override
        .filter(|base| !base.trim().is_empty())
        .unwrap_or(SIMPLE_HARNESS_SYSTEM_PREAMBLE);
    format!(
        "{preamble}\n\n# Packaged Browser Skill\n\n{}",
        PACKAGED_BROWSER_SKILL_MD.trim()
    )
}

pub fn shell_default_env(config: &ProviderRunConfig) -> HashMap<String, String> {
    if !config.options.simple_harness {
        return HashMap::new();
    }
    config.options.python_env.iter().cloned().collect()
}

pub fn max_turns_for_config(config: &ProviderRunConfig) -> Option<usize> {
    Some(config.options.max_turns)
}

pub fn mirror_existing_session(store: &Store, session_id: &str) -> Result<SimpleHarnessMirror> {
    let session = store
        .load_session(session_id)?
        .with_context(|| format!("unknown session id: {session_id}"))?;
    let paths = SimpleHarnessPaths::from_session(store.state_dir().to_path_buf(), &session);
    mirror_events_and_final(&paths, &store.events_for_session(session_id)?)
}

pub fn mirror_events_and_final(
    paths: &SimpleHarnessPaths,
    events: &[EventRecord],
) -> Result<SimpleHarnessMirror> {
    if let Some(parent) = paths.events_jsonl.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create events dir {}", parent.display()))?;
    }
    let mut events_file = fs::File::create(&paths.events_jsonl)
        .with_context(|| format!("create {}", paths.events_jsonl.display()))?;
    for event in events {
        serde_json::to_writer(&mut events_file, event)?;
        events_file.write_all(b"\n")?;
    }

    let final_result = session_result_from_events(events);
    let final_written = if let Some(result) = final_result.as_deref() {
        write_if_changed(&paths.final_txt, result)?;
        true
    } else {
        false
    };
    let final_bytes = final_result.as_ref().map_or(0, |result| result.len());
    Ok(SimpleHarnessMirror {
        events_written: events.len(),
        final_written,
        final_bytes,
        final_txt: paths.final_txt.clone(),
        events_jsonl: paths.events_jsonl.clone(),
    })
}

/// Append the mirror metadata after final capture.
///
/// This is intentionally separate from [`mirror_existing_session`]: the mirror
/// file should represent the model/tool run, while this store event records the
/// side-effect for the TUI/history.
pub fn record_mirror_event(
    store: &Store,
    session_id: &str,
    mirror: &SimpleHarnessMirror,
) -> Result<()> {
    store.append_event(
        session_id,
        SIMPLE_HARNESS_MIRRORED_EVENT,
        json!({
            "events_written": mirror.events_written,
            "final_written": mirror.final_written,
            "final_bytes": mirror.final_bytes,
            "final_txt": mirror.final_txt,
            "events_jsonl": mirror.events_jsonl,
        }),
    )?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimpleHarnessCleanup {
    pub daemon_present: bool,
    pub close_tab_attempted: bool,
    pub close_tab_ok: bool,
    pub reload_attempted: bool,
    pub reload_ok: bool,
    pub worker_present: bool,
    pub worker_shutdown_sent: bool,
    pub worker_stopped: bool,
    pub worker_events_jsonl_path: PathBuf,
    pub worker_events_count: usize,
    pub runtime_dir_removed: bool,
}

pub fn cleanup_existing_session(store: &Store, session_id: &str) -> Result<SimpleHarnessCleanup> {
    let session = store
        .load_session(session_id)?
        .with_context(|| format!("unknown session id: {session_id}"))?;
    let paths = SimpleHarnessPaths::from_session(store.state_dir().to_path_buf(), &session);
    Ok(cleanup_paths(&paths))
}

pub fn cleanup_paths(paths: &SimpleHarnessPaths) -> SimpleHarnessCleanup {
    let daemon_present = daemon_pid(&paths.runtime_dir).is_some();
    let close_tab_ok = if daemon_present {
        close_current_harness_tab(paths)
    } else {
        false
    };
    let reload_ok = if daemon_present {
        reload_harness_daemon(paths)
    } else {
        false
    };
    let worker_present = worker_pid(paths).is_some() || worker_ping(paths).is_some();
    let worker_stopped = stop_worker_for_paths(paths);
    let worker_events_count = count_file_lines(&paths.worker_events_jsonl_path);
    let runtime_dir_removed = fs::remove_dir_all(&paths.runtime_dir).is_ok();
    SimpleHarnessCleanup {
        daemon_present,
        close_tab_attempted: daemon_present,
        close_tab_ok,
        reload_attempted: daemon_present,
        reload_ok,
        worker_present,
        worker_shutdown_sent: worker_present,
        worker_stopped,
        worker_events_jsonl_path: paths.worker_events_jsonl_path.clone(),
        worker_events_count,
        runtime_dir_removed,
    }
}

pub fn record_cleanup_event(
    store: &Store,
    session_id: &str,
    cleanup: &SimpleHarnessCleanup,
) -> Result<()> {
    store.append_event(
        session_id,
        SIMPLE_HARNESS_CLEANED_EVENT,
        json!({
            "daemon_present": cleanup.daemon_present,
            "close_tab_attempted": cleanup.close_tab_attempted,
            "close_tab_ok": cleanup.close_tab_ok,
            "reload_attempted": cleanup.reload_attempted,
            "reload_ok": cleanup.reload_ok,
            "worker_present": cleanup.worker_present,
            "worker_shutdown_sent": cleanup.worker_shutdown_sent,
            "worker_stopped": cleanup.worker_stopped,
            "worker_events_jsonl": cleanup.worker_events_jsonl_path,
            "worker_events_count": cleanup.worker_events_count,
            "runtime_dir_removed": cleanup.runtime_dir_removed,
        }),
    )?;
    Ok(())
}

fn worker_children() -> &'static Mutex<HashMap<String, Child>> {
    WORKER_CHILDREN.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(unix)]
fn start_worker_for_paths(paths: &SimpleHarnessPaths) -> Result<SimpleHarnessWorker> {
    if let Some(pid) = worker_ping(paths) {
        return Ok(paths.worker_state(true, true, Some(pid)));
    }
    let key = paths.worker_socket_path.display().to_string();
    if let Ok(mut children) = worker_children().lock() {
        if let Some(mut child) = children.remove(&key) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    let _ = fs::remove_file(&paths.worker_socket_path);
    let _ = fs::remove_file(&paths.worker_pid_path);
    let _ = fs::remove_file(&paths.worker_events_jsonl_path);
    if let Some(parent) = paths.worker_log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create worker log dir {}", parent.display()))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.worker_log_path)
        .with_context(|| format!("open worker log {}", paths.worker_log_path.display()))?;
    let mut command = Command::new(&paths.browser_harness_worker_command_path);
    command
        .arg("--socket")
        .arg(&paths.worker_socket_path)
        .arg("--pid")
        .arg(&paths.worker_pid_path)
        .arg("--events")
        .arg(&paths.worker_events_jsonl_path)
        .current_dir(&paths.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            log.try_clone().context("clone worker stdout log")?,
        ))
        .stderr(Stdio::from(log));
    for (key, value) in paths.harness_env() {
        command.env(key, value);
    }
    let mut child = command.spawn().with_context(|| {
        format!(
            "spawn browser-harness worker {}",
            paths.browser_harness_worker_command_path.display()
        )
    })?;
    let child_pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(pid) = worker_ping(paths) {
            if let Ok(mut children) = worker_children().lock() {
                children.insert(key, child);
            }
            return Ok(paths.worker_state(true, false, Some(pid)));
        }
        if let Some(status) = child.try_wait().context("poll browser-harness worker")? {
            bail!(
                "browser-harness worker exited before listening with status {status}; log: {}",
                paths.worker_log_path.display()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    bail!(
        "browser-harness worker pid {child_pid} did not listen on {}; log: {}",
        paths.worker_socket_path.display(),
        paths.worker_log_path.display()
    )
}

#[cfg(not(unix))]
fn start_worker_for_paths(paths: &SimpleHarnessPaths) -> Result<SimpleHarnessWorker> {
    Ok(paths.worker_state(false, false, None))
}

#[cfg(unix)]
fn worker_request(
    paths: &SimpleHarnessPaths,
    payload: serde_json::Value,
) -> Option<serde_json::Value> {
    let mut stream = UnixStream::connect(&paths.worker_socket_path).ok()?;
    let timeout = Some(Duration::from_secs(2));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    let mut bytes = serde_json::to_vec(&payload).ok()?;
    bytes.push(b'\n');
    stream.write_all(&bytes).ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

#[cfg(not(unix))]
fn worker_request(
    _paths: &SimpleHarnessPaths,
    _payload: serde_json::Value,
) -> Option<serde_json::Value> {
    None
}

fn worker_ping(paths: &SimpleHarnessPaths) -> Option<u32> {
    let response = worker_request(paths, json!({"meta": "ping"}))?;
    if response.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return None;
    }
    response
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
}

fn worker_pid(paths: &SimpleHarnessPaths) -> Option<u32> {
    let pid = fs::read_to_string(&paths.worker_pid_path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    process_exists(pid).then_some(pid)
}

fn stop_worker_for_paths(paths: &SimpleHarnessPaths) -> bool {
    let key = paths.worker_socket_path.display().to_string();
    let shutdown_sent = worker_request(paths, json!({"meta": "shutdown"})).is_some();
    let child = worker_children()
        .lock()
        .ok()
        .and_then(|mut children| children.remove(&key));
    let mut stopped = false;
    if let Some(mut child) = child {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => {
                    stopped = true;
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }
        if !stopped {
            let _ = child.kill();
            stopped = child.wait().is_ok();
        }
    } else if let Some(pid) = worker_pid(paths) {
        stopped = shutdown_sent;
        if !stopped {
            let _ = Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .status();
            stopped = true;
        }
    } else {
        stopped = shutdown_sent;
    }
    let _ = fs::remove_file(&paths.worker_socket_path);
    let _ = fs::remove_file(&paths.worker_pid_path);
    stopped
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexCliRunSpec {
    pub prompt: String,
    pub model: String,
    pub paths: SimpleHarnessPaths,
    pub timeout_seconds: u64,
    pub extra_env: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexCliRunOutput {
    pub exit_code: Option<i32>,
    pub final_result: String,
    pub stdout_tail: String,
    pub stderr_tail: String,
}

/// Golden/reference adapter for parity with stock `codex exec`.
///
/// This is not the product path. It exists so evals can prove that
/// `SimpleHarness` still reproduces the known-good arm-C surface.
pub fn run_codex_cli_reference(spec: &CodexCliRunSpec) -> Result<CodexCliRunOutput> {
    spec.paths.ensure_prepared()?;
    if spec.prompt.trim().is_empty() {
        bail!("codex cli reference prompt must not be empty");
    }
    let mut command = Command::new("timeout");
    command
        .arg(spec.timeout_seconds.to_string())
        .arg("codex")
        .arg("exec")
        .arg("--cd")
        .arg(&spec.paths.cwd)
        .arg("--skip-git-repo-check")
        .arg("--dangerously-bypass-approvals-and-sandbox")
        .arg("-m")
        .arg(&spec.model)
        .arg("-o")
        .arg(&spec.paths.final_txt)
        .arg(&spec.prompt)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in spec.paths.harness_env() {
        command.env(key, value);
    }
    for (key, value) in &spec.extra_env {
        command.env(key, value);
    }
    let mut child = command.spawn().context("spawn codex cli reference run")?;
    let stdout = child.stdout.take().context("capture codex stdout")?;
    let stderr = child.stderr.take().context("capture codex stderr")?;
    let (tx, rx) = mpsc::channel();
    let tx_out = tx.clone();
    thread::spawn(move || {
        let text = read_to_string(stdout);
        let _ = tx_out.send(("stdout", text));
    });
    thread::spawn(move || {
        let text = read_to_string(stderr);
        let _ = tx.send(("stderr", text));
    });
    let status = child.wait().context("wait for codex cli reference run")?;
    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    for _ in 0..2 {
        let (stream, text) = rx.recv().context("read codex cli stream")?;
        if stream == "stdout" {
            stdout_text = text;
        } else {
            stderr_text = text;
        }
    }
    let final_result = fs::read_to_string(&spec.paths.final_txt).unwrap_or_default();
    Ok(CodexCliRunOutput {
        exit_code: status.code(),
        final_result,
        stdout_tail: tail_chars(&stdout_text, 20_000),
        stderr_tail: tail_chars(&stderr_text, 8_000),
    })
}

fn read_to_string(stream: impl std::io::Read) -> String {
    let mut output = String::new();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => output.push_str(&line),
            Err(_) => break,
        }
    }
    output
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let len = text.chars().count();
    if len <= max_chars {
        return text.to_string();
    }
    text.chars().skip(len - max_chars).collect()
}

fn sanitize_env_id(value: &str) -> String {
    let mut out: String = value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(60)
        .collect();
    if out.is_empty() {
        out.push_str("session");
    }
    out
}

fn short_hash(value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn browser_harness_path_value(harness_home: &Path) -> Option<String> {
    let separator = if cfg!(windows) { ';' } else { ':' };
    let existing_path = env::var(PATH_ENV).unwrap_or_default();
    let mut entries = Vec::new();
    push_path_entry(
        &mut entries,
        harness_home
            .join(".local")
            .join("bin")
            .display()
            .to_string(),
    );
    if let Ok(home) = env::var("HOME") {
        let local_bin = PathBuf::from(home).join(".local").join("bin");
        push_path_entry(&mut entries, local_bin.display().to_string());
    }
    for entry in existing_path
        .split(separator)
        .filter(|entry| !entry.is_empty())
    {
        push_path_entry(&mut entries, entry.to_string());
    }
    if entries.is_empty() {
        None
    } else {
        Some(entries.join(&separator.to_string()))
    }
}

fn browser_use_terminal_bin_value() -> Option<String> {
    if let Ok(path) = env::var(PRODUCT_CLI_BIN_ENV) {
        let path = path.trim();
        if !path.is_empty() {
            return Some(path.to_string());
        }
    }
    let current = env::current_exe().ok()?;
    if current
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "browser-use-terminal")
    {
        return Some(current.display().to_string());
    }
    let sibling = current
        .parent()
        .map(|parent| parent.join("browser-use-terminal"))?;
    if sibling.is_file() {
        Some(sibling.display().to_string())
    } else {
        None
    }
}

fn push_path_entry(entries: &mut Vec<String>, entry: String) {
    if entry.is_empty() || entries.iter().any(|existing| existing == &entry) {
        return;
    }
    entries.push(entry);
}

fn count_file_lines(path: &Path) -> usize {
    let Ok(file) = fs::File::open(path) else {
        return 0;
    };
    BufReader::new(file)
        .lines()
        .filter_map(std::result::Result::ok)
        .count()
}

fn simple_harness_browser_config(config: &ProviderRunConfig) -> serde_json::Value {
    json!({
        "browser_mode": trimmed_option(config.options.browser_mode.as_deref()),
        "browser_profile_id": trimmed_option(config.options.browser_profile_id.as_deref()),
        "browser_profile_label": trimmed_option(config.options.browser_profile_label.as_deref()),
        "browser_local_browser": trimmed_option(config.options.browser_local_browser.as_deref()),
    })
}

fn trimmed_option(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn copy_process_env_if_present(env: &mut Vec<(String, String)>, key: &str) {
    if let Ok(value) = std::env::var(key) {
        env.push((key.to_string(), value));
    }
}

fn daemon_pid(runtime_dir: &Path) -> Option<u32> {
    let pid = fs::read_to_string(runtime_dir.join("bu.pid"))
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    if process_exists(pid) {
        Some(pid)
    } else {
        None
    }
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    true
}

fn close_current_harness_tab(paths: &SimpleHarnessPaths) -> bool {
    let code = r#"
tab = current_tab()
url = tab.get("url") or ""
if url and not url.startswith(("chrome://", "chrome-untrusted://", "devtools://", "chrome-extension://", "about:")):
    cdp("Target.closeTarget", targetId=tab["targetId"])
"#;
    run_browser_harness_with_stdin(paths, code, 12)
}

fn reload_harness_daemon(paths: &SimpleHarnessPaths) -> bool {
    let mut command = timeout_command(25);
    command.arg("browser-harness").arg("--reload");
    apply_command_env(&mut command, paths);
    command
        .current_dir(&paths.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn run_browser_harness_with_stdin(
    paths: &SimpleHarnessPaths,
    stdin: &str,
    timeout_seconds: u64,
) -> bool {
    let mut command = timeout_command(timeout_seconds);
    command.arg("browser-harness");
    apply_command_env(&mut command, paths);
    command
        .current_dir(&paths.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return false,
    };
    if let Some(mut child_stdin) = child.stdin.take() {
        if child_stdin.write_all(stdin.as_bytes()).is_err() {
            let _ = child.kill();
            return false;
        }
    }
    child.wait().map(|status| status.success()).unwrap_or(false)
}

fn timeout_command(seconds: u64) -> Command {
    let mut command = Command::new("timeout");
    command.arg(seconds.to_string());
    command
}

fn apply_command_env(command: &mut Command, paths: &SimpleHarnessPaths) {
    for (key, value) in paths.harness_env() {
        command.env(key, value);
    }
}

fn upsert_env(env: &mut Vec<(String, String)>, key: &str, value: impl Into<String>) {
    let value = value.into();
    if let Some((_, existing)) = env.iter_mut().find(|(existing_key, _)| existing_key == key) {
        *existing = value;
        return;
    }
    env.push((key.to_string(), value));
}

fn write_if_changed(path: &Path, contents: &str) -> Result<bool> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir {}", parent.display()))?;
    }
    if fs::read_to_string(path).ok().as_deref() == Some(contents) {
        return Ok(false);
    }
    fs::write(path, contents).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

fn write_executable_if_changed(path: &Path, contents: &str) -> Result<bool> {
    let changed = write_if_changed(path, contents)?;
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("chmod +x {}", path.display()))?;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_use_protocol::SessionStatus;
    use browser_use_store::Store;
    use serde_json::json;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn session_meta(root: &Path) -> SessionMeta {
        SessionMeta {
            id: "sess-1".to_string(),
            parent_id: None,
            cwd: root.join("cwd").display().to_string(),
            artifact_root: root.join("artifacts").display().to_string(),
            status: SessionStatus::Created,
            created_ms: 1,
            updated_ms: 1,
        }
    }

    #[test]
    fn prepare_creates_codex_style_paths_and_skill() {
        let dir = tempfile::tempdir().unwrap();
        let meta = session_meta(dir.path());
        let paths = SimpleHarnessPaths::from_session(dir.path().join("state"), &meta);
        let prepared = paths.ensure_prepared().unwrap();
        assert!(prepared.skill_changed);
        assert!(paths.cwd.is_dir());
        assert!(paths.home.is_dir());
        assert!(paths.agent_workspace.is_dir());
        assert!(paths
            .agent_workspace
            .join(AGENT_HELPERS_FILE_NAME)
            .is_file());
        assert!(paths.domain_skills_root.is_dir());
        assert!(paths.runtime_dir.is_dir());
        assert!(paths.tmp_dir.is_dir());
        assert!(paths.browser_skill_path.is_file());
        assert!(paths.browser_command_path.is_file());
        assert!(paths.browser_harness_command_path.is_file());
        assert!(paths.browser_harness_worker_command_path.is_file());
        assert!(paths.browser_harness_worker_client_command_path.is_file());
        assert!(paths.artifact_audit_command_path.is_file());
        assert_eq!(
            paths.worker_events_jsonl_path,
            paths.tmp_dir.join("browser-harness-worker-events.jsonl")
        );
        assert!(!paths.worker_events_jsonl_path.exists());
        assert!(!paths.force_cloud_marker_path.exists());
        assert!(fs::read_to_string(&paths.browser_skill_path)
            .unwrap()
            .contains("browser-harness"));
        let skill = fs::read_to_string(&paths.browser_skill_path).unwrap();
        assert!(skill.contains("browser-harness <<'PY'"));
        assert!(skill.contains("run.py calls ensure_daemon() before exec"));
        assert!(skill.contains("start_remote_daemon(\"work\")"));
        assert_eq!(
            fs::read_to_string(&paths.browser_command_path).unwrap(),
            BROWSER_COMMAND_SHIM
        );
        assert_eq!(
            fs::read_to_string(&paths.browser_harness_command_path).unwrap(),
            BROWSER_HARNESS_COMMAND_SHIM
        );
        assert_eq!(
            fs::read_to_string(&paths.browser_harness_worker_command_path).unwrap(),
            BROWSER_HARNESS_WORKER_PY
        );
        assert_eq!(
            fs::read_to_string(&paths.browser_harness_worker_client_command_path).unwrap(),
            BROWSER_HARNESS_WORKER_CLIENT_SHIM
        );
        assert!(fs::read_to_string(&paths.artifact_audit_command_path)
            .unwrap()
            .contains("artifact-audit"));
        let agent_helpers =
            fs::read_to_string(paths.agent_workspace.join(AGENT_HELPERS_FILE_NAME)).unwrap();
        assert!(agent_helpers.contains("def nav_policy()"));
        assert!(agent_helpers.contains("def email_address()"));
        assert!(agent_helpers.contains("def email_inbox("));
        assert!(agent_helpers.contains("def email_message("));
        assert!(agent_helpers.contains("def available_secrets()"));
        assert!(agent_helpers.contains("def secret("));
        assert!(agent_helpers.contains("def totp("));
        assert!(agent_helpers.contains("def type_text("));
        assert!(agent_helpers.contains("def fill_input("));
        assert!(agent_helpers.contains("secret.resolve"));
        assert!(agent_helpers.contains("secrets\", \"email\", \"inbox"));
        assert!(agent_helpers.contains("BU_BROWSER_ALLOWED_DOMAINS"));
        assert!(agent_helpers.contains(PRODUCT_SECRET_META_ENV));
        assert!(agent_helpers.contains(PRODUCT_STATE_DIR_ENV));
        #[cfg(unix)]
        {
            let mode = fs::metadata(&paths.browser_command_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755);
            let mode = fs::metadata(&paths.browser_harness_command_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755);
            let mode = fs::metadata(&paths.artifact_audit_command_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755);
            let mode = fs::metadata(&paths.browser_harness_worker_command_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755);
            let mode = fs::metadata(&paths.browser_harness_worker_client_command_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755);
        }

        let prepared_again = paths.ensure_prepared().unwrap();
        assert!(!prepared_again.skill_changed);

        assert!(paths.sync_force_cloud_marker(true).unwrap());
        assert_eq!(
            fs::read_to_string(&paths.force_cloud_marker_path).unwrap(),
            "1\n"
        );
        assert!(!paths.sync_force_cloud_marker(true).unwrap());
        assert!(paths.sync_force_cloud_marker(false).unwrap());
        assert!(!paths.force_cloud_marker_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn browser_harness_shim_preserves_stdout_stderr_exit_and_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let meta = session_meta(dir.path());
        let paths = SimpleHarnessPaths::from_session(dir.path().join("state"), &meta);
        paths.ensure_prepared().unwrap();
        let fake_bin = dir.path().join("fake-bin");
        fs::create_dir_all(&fake_bin).unwrap();
        let fake_harness = fake_bin.join("browser-harness");
        fs::write(
            &fake_harness,
            r#"#!/usr/bin/env bash
printf 'stdout:%s:%s:' "$1" "$2"
cat
printf 'stderr:%s:%s\n' "$1" "$2" >&2
exit 42
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_harness).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        fs::set_permissions(&fake_harness, permissions).unwrap();

        let harness_bin = paths.home.join(".local").join("bin");
        let existing_path = env::var(PATH_ENV).unwrap_or_default();
        let output = Command::new(&paths.browser_harness_command_path)
            .arg("alpha")
            .arg("beta")
            .env(
                "PATH",
                format!(
                    "{}:{}:{}",
                    harness_bin.display(),
                    fake_bin.display(),
                    existing_path
                ),
            )
            .env_remove("BU_FORCE_CLOUD")
            .env_remove("LLM_BROWSER_BROWSER_MODE")
            .env_remove("BROWSER_USE_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child.stdin.as_mut().unwrap().write_all(b"stdin-body")?;
                child.wait_with_output()
            })
            .unwrap();

        assert_eq!(output.status.code(), Some(42));
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "stdout:alpha:beta:stdin-body"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "stderr:alpha:beta\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn browser_harness_shim_exports_source_override_to_real_harness() {
        let dir = tempfile::tempdir().unwrap();
        let meta = session_meta(dir.path());
        let paths = SimpleHarnessPaths::from_session(dir.path().join("state"), &meta);
        paths.ensure_prepared().unwrap();
        let fake_bin = dir.path().join("fake-bin");
        let harness_src = dir.path().join("browser-harness-src");
        fs::create_dir_all(&fake_bin).unwrap();
        fs::create_dir_all(&harness_src).unwrap();
        let fake_harness = fake_bin.join("browser-harness");
        fs::write(
            &fake_harness,
            "#!/usr/bin/env bash\nprintf '%s\\n' \"${PYTHONPATH%%:*}\"\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_harness).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        fs::set_permissions(&fake_harness, permissions).unwrap();

        let harness_bin = paths.home.join(".local").join("bin");
        let existing_path = env::var(PATH_ENV).unwrap_or_default();
        let output = Command::new(&paths.browser_harness_command_path)
            .env(
                "PATH",
                format!(
                    "{}:{}:{}",
                    harness_bin.display(),
                    fake_bin.display(),
                    existing_path
                ),
            )
            .env("BROWSER_HARNESS_SRC", harness_src.display().to_string())
            .env("PYTHONPATH", "existing-pythonpath")
            .env_remove("BU_FORCE_CLOUD")
            .env_remove("LLM_BROWSER_BROWSER_MODE")
            .env_remove("BROWSER_USE_API_KEY")
            .output()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            harness_src.display().to_string()
        );
    }

    #[cfg(unix)]
    #[test]
    fn browser_harness_cloud_bootstrap_forwards_selected_profile_id() {
        let dir = tempfile::tempdir().unwrap();
        let meta = session_meta(dir.path());
        let paths = SimpleHarnessPaths::from_session(dir.path().join("state"), &meta);
        paths.ensure_prepared().unwrap();
        paths.sync_force_cloud_marker(true).unwrap();

        let fake_bin = dir.path().join("fake-bin");
        let fake_src = dir.path().join("fake-src");
        let fake_pkg = fake_src.join("browser_harness");
        fs::create_dir_all(&fake_bin).unwrap();
        fs::create_dir_all(&fake_pkg).unwrap();
        fs::write(fake_pkg.join("__init__.py"), "").unwrap();
        fs::write(
            fake_pkg.join("_ipc.py"),
            "def log_path(name):\n    return ''\n",
        )
        .unwrap();
        fs::write(
            fake_pkg.join("admin.py"),
            r#"import json
import os

NAME = "fake-name"

def daemon_alive(name=None):
    return False

def restart_daemon(name=None):
    raise AssertionError("restart_daemon should not be called")

def start_remote_daemon(name, **kwargs):
    with open(os.environ["BH_TEST_CALLS"], "w") as f:
        json.dump({"name": name, "kwargs": kwargs}, f, sort_keys=True)
"#,
        )
        .unwrap();
        let fake_harness = fake_bin.join("browser-harness");
        fs::write(
            &fake_harness,
            "#!/usr/bin/env bash\nprintf 'real harness ran\\n'\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_harness).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        fs::set_permissions(&fake_harness, permissions).unwrap();
        let calls = dir.path().join("calls.json");

        let harness_bin = paths.home.join(".local").join("bin");
        let existing_path = env::var(PATH_ENV).unwrap_or_default();
        let output = Command::new(&paths.browser_harness_command_path)
            .env(
                "PATH",
                format!(
                    "{}:{}:{}",
                    harness_bin.display(),
                    fake_bin.display(),
                    existing_path
                ),
            )
            .env("BROWSER_HARNESS_SRC", fake_src.display().to_string())
            .env("BROWSER_USE_API_KEY", "test-key")
            .env("BH_TEST_CALLS", calls.display().to_string())
            .env("BU_AUTOSPAWN_PROFILE_ID", "profile-123")
            .env("BU_AUTOSPAWN_PROFILE_NAME", "ignored-name")
            .env("BH_CLOUD_TIMEOUT_MINUTES", "37")
            .env_remove(WORKER_SOCKET_ENV)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "real harness ran\n"
        );
        let call: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(calls).unwrap()).unwrap();
        assert_eq!(call["name"], "fake-name");
        assert_eq!(call["kwargs"]["profileId"], "profile-123");
        assert_eq!(call["kwargs"]["timeout"], 37);
        assert!(call["kwargs"].get("profileName").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn browser_harness_cloud_bootstrap_restarts_stale_remote_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let meta = session_meta(dir.path());
        let paths = SimpleHarnessPaths::from_session(dir.path().join("state"), &meta);
        paths.ensure_prepared().unwrap();
        paths.sync_force_cloud_marker(true).unwrap();

        let fake_bin = dir.path().join("fake-bin");
        let fake_src = dir.path().join("fake-src");
        let fake_pkg = fake_src.join("browser_harness");
        fs::create_dir_all(&fake_bin).unwrap();
        fs::create_dir_all(&fake_pkg).unwrap();
        fs::write(fake_pkg.join("__init__.py"), "").unwrap();
        fs::write(
            fake_pkg.join("_ipc.py"),
            r#"import os

class FakeConn:
    def close(self):
        pass

def log_path(name):
    return os.environ["BH_TEST_LOG"]

def connect(name, timeout=1.0):
    return FakeConn(), None

def request(c, token, req):
    return {"error": "stale remote cdp"}
"#,
        )
        .unwrap();
        fs::write(
            fake_pkg.join("admin.py"),
            r#"import json
import os

NAME = "fake-name"

def _state():
    try:
        return json.loads(open(os.environ["BH_TEST_STATE"]).read())
    except FileNotFoundError:
        return {"alive": True, "restarts": 0, "starts": []}

def _write(state):
    with open(os.environ["BH_TEST_STATE"], "w") as f:
        json.dump(state, f, sort_keys=True)

def daemon_alive(name=None):
    return _state().get("alive", False)

def restart_daemon(name=None):
    state = _state()
    state["alive"] = False
    state["restarts"] = state.get("restarts", 0) + 1
    _write(state)

def start_remote_daemon(name, **kwargs):
    state = _state()
    state["alive"] = True
    state.setdefault("starts", []).append({"name": name, "kwargs": kwargs})
    _write(state)
"#,
        )
        .unwrap();
        let fake_harness = fake_bin.join("browser-harness");
        fs::write(
            &fake_harness,
            "#!/usr/bin/env bash\nprintf 'real harness ran\\n'\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_harness).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        fs::set_permissions(&fake_harness, permissions).unwrap();
        let state_path = dir.path().join("state.json");
        let log_path = dir.path().join("bu.log");
        fs::write(&state_path, r#"{"alive":true,"restarts":0,"starts":[]}"#).unwrap();
        fs::write(&log_path, "listening on fake remote=browser-123\n").unwrap();

        let harness_bin = paths.home.join(".local").join("bin");
        let existing_path = env::var(PATH_ENV).unwrap_or_default();
        let output = Command::new(&paths.browser_harness_command_path)
            .env(
                "PATH",
                format!(
                    "{}:{}:{}",
                    harness_bin.display(),
                    fake_bin.display(),
                    existing_path
                ),
            )
            .env("BROWSER_HARNESS_SRC", fake_src.display().to_string())
            .env("BROWSER_USE_API_KEY", "test-key")
            .env("BH_TEST_STATE", state_path.display().to_string())
            .env("BH_TEST_LOG", log_path.display().to_string())
            .env("BH_CLOUD_TIMEOUT_MINUTES", "37")
            .env_remove(WORKER_SOCKET_ENV)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "real harness ran\n"
        );
        let state: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(state_path).unwrap()).unwrap();
        assert_eq!(state["restarts"], 1);
        assert_eq!(state["starts"].as_array().unwrap().len(), 1);
        assert_eq!(state["starts"][0]["name"], "fake-name");
        assert_eq!(state["starts"][0]["kwargs"]["timeout"], 37);
    }

    #[cfg(unix)]
    #[test]
    fn browser_harness_shim_uses_supervised_worker_when_available() {
        let dir = tempfile::tempdir().unwrap();
        let meta = session_meta(dir.path());
        let paths = SimpleHarnessPaths::from_session(dir.path().join("state"), &meta);
        paths.ensure_prepared().unwrap();
        let worker = paths.start_worker().unwrap();
        assert!(worker.enabled);
        let fake_bin = dir.path().join("fake-bin");
        fs::create_dir_all(&fake_bin).unwrap();
        let fake_harness = fake_bin.join("browser-harness");
        fs::write(
            &fake_harness,
            format!(
                r#"#!/usr/bin/env bash
if [ "${{{}}}" != "1" ]; then
  echo "not routed through worker" >&2
  exit 99
fi
printf 'worker-stdout:%s:%s:' "$1" "$2"
cat
printf 'worker-stderr:%s:%s\n' "$1" "$2" >&2
exit 43
"#,
                WORKER_ACTIVE_ENV
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_harness).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        fs::set_permissions(&fake_harness, permissions).unwrap();

        let harness_bin = paths.home.join(".local").join("bin");
        let existing_path = env::var(PATH_ENV).unwrap_or_default();
        let output = Command::new(&paths.browser_harness_command_path)
            .arg("alpha")
            .arg("beta")
            .env(
                "PATH",
                format!(
                    "{}:{}:{}",
                    harness_bin.display(),
                    fake_bin.display(),
                    existing_path
                ),
            )
            .env(
                WORKER_SOCKET_ENV,
                paths.worker_socket_path.display().to_string(),
            )
            .env_remove("BU_FORCE_CLOUD")
            .env_remove("LLM_BROWSER_BROWSER_MODE")
            .env_remove("BROWSER_USE_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child.stdin.as_mut().unwrap().write_all(b"stdin-body")?;
                child.wait_with_output()
            })
            .unwrap();

        assert_eq!(output.status.code(), Some(43));
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "worker-stdout:alpha:beta:stdin-body"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "worker-stderr:alpha:beta\n"
        );
        let worker_events = fs::read_to_string(&paths.worker_events_jsonl_path).unwrap();
        assert!(worker_events.contains("\"event\": \"worker.started\""));
        assert!(worker_events.contains("\"event\": \"request.started\""));
        assert!(worker_events.contains("\"event\": \"request.finished\""));
        assert!(worker_events.contains("\"exit_code\": 43"));
        assert!(worker_events.contains("\"stdin_bytes\": 10"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("request.finished"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("request.finished"));
        let cleanup = cleanup_paths(&paths);
        assert!(cleanup.worker_present);
        assert!(cleanup.worker_stopped);
        assert_eq!(
            cleanup.worker_events_jsonl_path,
            paths.worker_events_jsonl_path
        );
        assert!(cleanup.worker_events_count >= worker_events.lines().count());
    }

    #[cfg(unix)]
    #[test]
    fn browser_harness_worker_matches_direct_harness_output_for_fixed_trace() {
        let dir = tempfile::tempdir().unwrap();
        let meta = session_meta(dir.path());
        let paths = SimpleHarnessPaths::from_session(dir.path().join("state"), &meta);
        paths.ensure_prepared().unwrap();
        let worker = paths.start_worker().unwrap();
        assert!(worker.enabled);

        let fake_bin = dir.path().join("fake-bin");
        fs::create_dir_all(&fake_bin).unwrap();
        let fake_harness = fake_bin.join("browser-harness");
        fs::write(
            &fake_harness,
            r#"#!/usr/bin/env bash
printf 'stdout-start\n'
printf 'argc=%s\n' "$#"
i=0
for arg in "$@"; do
  printf 'arg%s=%s\n' "$i" "$arg"
  i=$((i + 1))
done
printf 'custom=%s\n' "${BH_TRACE_CUSTOM:-}"
printf 'stdin<<'
cat
printf '>>\n'
printf 'stderr-start\n' >&2
printf 'stderr-argc=%s\n' "$#" >&2
printf 'stderr-custom=%s\n' "${BH_TRACE_CUSTOM:-}" >&2
exit 17
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_harness).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        fs::set_permissions(&fake_harness, permissions).unwrap();

        let stdin = b"line one\nline two with spaces\n";
        let args = ["observe", "--label", "A B"];
        let run_with_stdin = |mut command: Command| {
            command
                .args(args)
                .env("BH_TRACE_CUSTOM", "trace-value")
                .env_remove("BU_FORCE_CLOUD")
                .env_remove("LLM_BROWSER_BROWSER_MODE")
                .env_remove("BROWSER_USE_API_KEY")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    child.stdin.as_mut().unwrap().write_all(stdin)?;
                    child.wait_with_output()
                })
                .unwrap()
        };

        let direct = run_with_stdin(Command::new(&fake_harness));
        let harness_bin = paths.home.join(".local").join("bin");
        let existing_path = env::var(PATH_ENV).unwrap_or_default();
        let mut supervised_command = Command::new(&paths.browser_harness_command_path);
        supervised_command
            .env(
                "PATH",
                format!(
                    "{}:{}:{}",
                    harness_bin.display(),
                    fake_bin.display(),
                    existing_path
                ),
            )
            .env(
                WORKER_SOCKET_ENV,
                paths.worker_socket_path.display().to_string(),
            );
        let supervised = run_with_stdin(supervised_command);

        assert_eq!(supervised.status.code(), direct.status.code());
        assert_eq!(supervised.stdout, direct.stdout);
        assert_eq!(supervised.stderr, direct.stderr);
        let worker_events = fs::read_to_string(&paths.worker_events_jsonl_path).unwrap();
        assert!(worker_events.contains("\"event\": \"request.finished\""));
        assert!(worker_events.contains("\"exit_code\": 17"));
        assert!(!String::from_utf8_lossy(&supervised.stdout).contains("request.finished"));
        assert!(!String::from_utf8_lossy(&supervised.stderr).contains("request.finished"));
        let cleanup = cleanup_paths(&paths);
        assert!(cleanup.worker_stopped);
    }

    #[cfg(unix)]
    #[test]
    fn browser_harness_worker_redacts_resolved_secret_output() {
        use base64::{engine::general_purpose::STANDARD, Engine as _};

        let dir = tempfile::tempdir().unwrap();
        let meta = session_meta(dir.path());
        let paths = SimpleHarnessPaths::from_session(dir.path().join("state"), &meta);
        paths.ensure_prepared().unwrap();
        let worker = paths.start_worker().unwrap();
        assert!(worker.enabled);

        let fake_cli = dir.path().join("browser-use-terminal");
        fs::write(
            &fake_cli,
            r#"#!/usr/bin/env bash
printf '{"value":"hunter2pass","label":"password"}\n'
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_cli).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        fs::set_permissions(&fake_cli, permissions).unwrap();

        let secret = worker_request(
            &paths,
            json!({
                "meta": "secret.resolve",
                "state_dir": paths.state_dir,
                "cli": fake_cli,
                "domain": "github.com",
                "name": "password",
            }),
        )
        .expect("secret response");
        assert_eq!(secret["ok"], true);
        assert_eq!(secret["value"], "hunter2pass");

        let fake_harness = dir.path().join("browser-harness-real");
        fs::write(
            &fake_harness,
            r#"#!/usr/bin/env bash
printf 'stdout hunter2pass\n'
printf 'stderr hunter2pass\n' >&2
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_harness).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        fs::set_permissions(&fake_harness, permissions).unwrap();

        let response = worker_request(
            &paths,
            json!({
                "real_browser_harness": fake_harness,
                "argv": [],
                "stdin_b64": "",
                "env": paths.harness_env().into_iter().collect::<std::collections::HashMap<_, _>>(),
            }),
        )
        .expect("worker run response");
        let stdout = String::from_utf8(
            STANDARD
                .decode(response["stdout_b64"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        let stderr = String::from_utf8(
            STANDARD
                .decode(response["stderr_b64"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stdout, "stdout <secret>password</secret>\n");
        assert_eq!(stderr, "stderr <secret>password</secret>\n");
        let cleanup = cleanup_paths(&paths);
        assert!(cleanup.worker_stopped);
    }

    #[test]
    fn mirror_uses_session_done_result_not_result_files() {
        let dir = tempfile::tempdir().unwrap();
        let meta = session_meta(dir.path());
        let paths = SimpleHarnessPaths::from_session(dir.path().join("state"), &meta);
        paths.ensure_prepared().unwrap();
        fs::write(paths.cwd.join("result.json"), "{\"wrong\":true}").unwrap();
        let events = vec![EventRecord {
            seq: 1,
            id: "e1".to_string(),
            session_id: meta.id.clone(),
            ts_ms: 1,
            event_type: "session.done".to_string(),
            payload: json!({"result": "final assistant answer"}),
        }];
        let mirror = mirror_events_and_final(&paths, &events).unwrap();
        assert!(mirror.final_written);
        assert_eq!(
            fs::read_to_string(paths.final_txt).unwrap(),
            "final assistant answer"
        );
        assert!(fs::read_to_string(paths.events_jsonl)
            .unwrap()
            .contains("\"session.done\""));
    }

    #[test]
    fn apply_to_config_sets_harness_python_env() {
        let dir = tempfile::tempdir().unwrap();
        let meta = session_meta(dir.path());
        let paths = SimpleHarnessPaths::from_session(dir.path().join("state"), &meta);
        let mut config =
            ProviderRunConfig::new(crate::config_overrides::ProviderBackend::Fake, "fake");
        config.options.browser_mode = Some("cloud".to_string());
        config.options.browser_profile_id = Some("cloud-profile-123".to_string());
        config.options.browser_profile_label = Some("Work Cloud".to_string());
        config.options.browser_local_browser = Some("Google Chrome".to_string());
        config
            .options
            .python_env
            .push(("BH_DOMAIN_SKILLS".to_string(), "1".to_string()));
        paths.apply_to_config(&mut config);
        let env = config
            .options
            .python_env
            .iter()
            .cloned()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            env.get("BH_AGENT_WORKSPACE"),
            Some(&paths.agent_workspace.display().to_string())
        );
        assert_eq!(
            env.get("BH_DOMAIN_SKILLS_ROOT"),
            Some(&paths.domain_skills_root.display().to_string())
        );
        assert_eq!(env.get("BH_DOMAIN_SKILLS"), Some(&"0".to_string()));
        assert_eq!(
            env.get("BH_RUNTIME_DIR"),
            Some(&paths.runtime_dir.display().to_string())
        );
        assert_eq!(
            env.get("BH_TMP_DIR"),
            Some(&paths.tmp_dir.display().to_string())
        );
        assert_eq!(env.get("BU_AUTOSPAWN"), Some(&"1".to_string()));
        assert_eq!(env.get("BU_FORCE_CLOUD"), Some(&"1".to_string()));
        assert_eq!(
            env.get(PRODUCT_BROWSER_MODE_ENV),
            Some(&"cloud".to_string())
        );
        assert_eq!(
            env.get(PRODUCT_BROWSER_PROFILE_ID_ENV),
            Some(&"cloud-profile-123".to_string())
        );
        assert_eq!(
            env.get(PRODUCT_BROWSER_PROFILE_LABEL_ENV),
            Some(&"Work Cloud".to_string())
        );
        assert_eq!(
            env.get(PRODUCT_BROWSER_LOCAL_BROWSER_ENV),
            Some(&"Google Chrome".to_string())
        );
        assert_eq!(
            env.get(PRODUCT_STATE_DIR_ENV),
            Some(&paths.state_dir.display().to_string())
        );
        assert_eq!(
            env.get(CLOUD_AUTOSPAWN_PROFILE_ID_ENV),
            Some(&"cloud-profile-123".to_string())
        );
        assert!(!env.contains_key(CLOUD_AUTOSPAWN_PROFILE_NAME_ENV));
        assert_eq!(
            env.get("LLM_BROWSER_BROWSER_MODE"),
            Some(&"cloud".to_string())
        );
        assert_eq!(env.get("BU_CDP_URL"), Some(&"".to_string()));
        assert_eq!(env.get("BU_CDP_WS"), Some(&"".to_string()));
        assert_eq!(env.get("BU_BROWSER_ID"), Some(&"".to_string()));
        assert_eq!(
            env.get("BU_NAME"),
            Some(&format!("sh{}", sanitize_env_id(&meta.id)))
        );
        assert_eq!(
            env.get("CODEX_HOME"),
            Some(&paths.home.display().to_string())
        );
        if let Some(cli_bin) = browser_use_terminal_bin_value() {
            assert_eq!(env.get(PRODUCT_CLI_BIN_ENV), Some(&cli_bin));
        }
        let path = env.get("PATH").expect("PATH env");
        assert!(path.contains(&paths.home.join(".local/bin").display().to_string()));
        assert!(path.contains(".local/bin"));
    }

    #[test]
    fn apply_to_config_uses_cloud_profile_name_when_id_absent() {
        let dir = tempfile::tempdir().unwrap();
        let meta = session_meta(dir.path());
        let paths = SimpleHarnessPaths::from_session(dir.path().join("state"), &meta);
        let mut config =
            ProviderRunConfig::new(crate::config_overrides::ProviderBackend::Fake, "fake");
        config.options.browser_mode = Some("cloud".to_string());
        config.options.browser_profile_label = Some("Work Cloud".to_string());

        paths.apply_to_config(&mut config);

        let env = config
            .options
            .python_env
            .iter()
            .cloned()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            env.get(CLOUD_AUTOSPAWN_PROFILE_NAME_ENV),
            Some(&"Work Cloud".to_string())
        );
        assert!(!env.contains_key(CLOUD_AUTOSPAWN_PROFILE_ID_ENV));
    }

    #[test]
    fn apply_to_config_carries_local_profile_without_cloud_autospawn() {
        let dir = tempfile::tempdir().unwrap();
        let meta = session_meta(dir.path());
        let paths = SimpleHarnessPaths::from_session(dir.path().join("state"), &meta);
        let mut config =
            ProviderRunConfig::new(crate::config_overrides::ProviderBackend::Fake, "fake");
        config.options.browser_mode = Some("local".to_string());
        config.options.browser_profile_id = Some("google-chrome:Profile 1".to_string());
        config.options.browser_local_browser = Some("Google Chrome".to_string());

        paths.apply_to_config(&mut config);

        let env = config
            .options
            .python_env
            .iter()
            .cloned()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            env.get(PRODUCT_BROWSER_MODE_ENV),
            Some(&"local".to_string())
        );
        assert_eq!(
            env.get(PRODUCT_BROWSER_PROFILE_ID_ENV),
            Some(&"google-chrome:Profile 1".to_string())
        );
        assert_eq!(
            env.get(PRODUCT_BROWSER_LOCAL_BROWSER_ENV),
            Some(&"Google Chrome".to_string())
        );
        assert!(!env.contains_key(CLOUD_AUTOSPAWN_PROFILE_ID_ENV));
        assert!(!env.contains_key(CLOUD_AUTOSPAWN_PROFILE_NAME_ENV));
    }

    #[test]
    fn simple_harness_prompt_and_shell_env_are_opt_in() {
        let prompt = simple_harness_system_prompt(None);
        assert!(prompt.contains("browser-harness"));
        assert!(prompt.contains("Do not fabricate"));

        let mut config =
            ProviderRunConfig::new(crate::config_overrides::ProviderBackend::Fake, "fake");
        config
            .options
            .python_env
            .push(("BU_AUTOSPAWN".to_string(), "1".to_string()));
        assert!(shell_default_env(&config).is_empty());

        config.options.simple_harness = true;
        let env = shell_default_env(&config);
        assert_eq!(env.get("BU_AUTOSPAWN"), Some(&"1".to_string()));
    }

    #[test]
    fn max_turns_still_honors_run_config_in_simple_harness() {
        let mut config =
            ProviderRunConfig::new(crate::config_overrides::ProviderBackend::Fake, "fake");
        config.options.simple_harness = true;
        config.options.max_turns = 123;
        assert_eq!(max_turns_for_config(&config), Some(123));
    }

    #[test]
    fn prepare_and_mirror_existing_session_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("state")).unwrap();
        let session = store
            .create_session_with_id_and_artifact_root(
                None,
                dir.path().join("cwd"),
                dir.path().join("artifacts"),
                "sess-1".to_string(),
            )
            .unwrap();
        let mut config =
            ProviderRunConfig::new(crate::config_overrides::ProviderBackend::Fake, "fake");
        let prepared = prepare_existing_session(&store, &session.id, &mut config).unwrap();
        assert!(prepared.worker.enabled);
        assert!(prepared.worker.pid.is_some());
        store
            .append_event(&session.id, "session.done", json!({"result": "done"}))
            .unwrap();
        let mirror = mirror_existing_session(&store, &session.id).unwrap();
        record_mirror_event(&store, &session.id, &mirror).unwrap();
        assert!(prepared.paths.browser_skill_path.is_file());
        assert!(!prepared.paths.force_cloud_marker_path.exists());
        assert_eq!(
            fs::read_to_string(prepared.paths.final_txt).unwrap(),
            "done"
        );
        assert!(store
            .events_for_session(&session.id)
            .unwrap()
            .iter()
            .any(|event| event.event_type == SIMPLE_HARNESS_MIRRORED_EVENT));
        let cleanup = cleanup_existing_session(&store, &session.id).unwrap();
        assert!(cleanup.worker_present);
        assert!(cleanup.worker_stopped);
        assert!(cleanup.worker_events_count >= 3);
        record_cleanup_event(&store, &session.id, &cleanup).unwrap();
        let cleanup_event = store
            .events_for_session(&session.id)
            .unwrap()
            .into_iter()
            .find(|event| event.event_type == SIMPLE_HARNESS_CLEANED_EVENT)
            .expect("harness cleanup event");
        assert!(
            cleanup_event
                .payload
                .get("worker_events_count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default()
                >= 3
        );
    }

    #[test]
    fn prepare_existing_session_applies_domain_policy_to_harness_env() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("state")).unwrap();
        let session = store
            .create_session_with_id_and_artifact_root(
                None,
                dir.path().join("cwd"),
                dir.path().join("artifacts"),
                "sess-1".to_string(),
            )
            .unwrap();
        crate::tools::handlers::secrets_admin::add_domain(&store, "Example.com", true).unwrap();
        crate::tools::handlers::secrets_admin::add_domain(&store, "tracking.example", false)
            .unwrap();
        let mut config =
            ProviderRunConfig::new(crate::config_overrides::ProviderBackend::Fake, "fake");

        prepare_existing_session(&store, &session.id, &mut config).unwrap();

        let env = config
            .options
            .python_env
            .iter()
            .cloned()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            env.get("BU_BROWSER_ALLOWED_DOMAINS"),
            Some(&"[\"example.com\"]".to_string())
        );
        assert_eq!(
            env.get("BU_BROWSER_PROHIBITED_DOMAINS"),
            Some(&"[\"tracking.example\"]".to_string())
        );
        let prepared_event = store
            .events_for_session(&session.id)
            .unwrap()
            .into_iter()
            .find(|event| event.event_type == SIMPLE_HARNESS_PREPARED_EVENT)
            .expect("harness prepared event");
        assert_eq!(
            prepared_event.payload["domain_policy"]["allowed_domains"],
            json!(["example.com"])
        );
        assert_eq!(
            prepared_event.payload["domain_policy"]["denied_domains"],
            json!(["tracking.example"])
        );
    }

    #[test]
    fn prepare_existing_session_applies_secret_metadata_and_allowlist_union() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("state")).unwrap();
        let session = store
            .create_session_with_id_and_artifact_root(
                None,
                dir.path().join("cwd"),
                dir.path().join("artifacts"),
                "sess-1".to_string(),
            )
            .unwrap();
        crate::tools::handlers::secrets_admin::add_domain(&store, "example.com", true).unwrap();
        crate::tools::handlers::secrets_admin::set_secret_active(
            &store,
            "github.com",
            "password",
            browser_use_secrets::SecretKind::Password,
            vec!["*.okta.com".to_string()],
            "hunter2pass",
        )
        .unwrap();
        let mut config =
            ProviderRunConfig::new(crate::config_overrides::ProviderBackend::Fake, "fake");

        prepare_existing_session(&store, &session.id, &mut config).unwrap();

        let env = config
            .options
            .python_env
            .iter()
            .cloned()
            .collect::<std::collections::HashMap<_, _>>();
        let allow: Vec<String> =
            serde_json::from_str(env.get("BU_BROWSER_ALLOWED_DOMAINS").unwrap()).unwrap();
        assert_eq!(
            allow,
            vec![
                "*.okta.com".to_string(),
                "example.com".to_string(),
                "github.com".to_string(),
            ]
        );
        let meta: serde_json::Value =
            serde_json::from_str(env.get(PRODUCT_SECRET_META_ENV).unwrap()).unwrap();
        assert_eq!(meta["github.com"]["password"]["totp"], false);
        assert_eq!(meta["*.okta.com"]["password"]["totp"], false);
        assert!(!env
            .get(PRODUCT_SECRET_META_ENV)
            .unwrap()
            .contains("hunter2pass"));
        let prepared_event = store
            .events_for_session(&session.id)
            .unwrap()
            .into_iter()
            .find(|event| event.event_type == SIMPLE_HARNESS_PREPARED_EVENT)
            .expect("harness prepared event");
        assert_eq!(prepared_event.payload["secret_policy"]["secret_count"], 1);
        assert_eq!(prepared_event.payload["secret_policy"]["env_applied"], true);
    }

    #[test]
    fn prepare_existing_session_writes_force_cloud_marker_for_cloud_mode() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("state")).unwrap();
        let session = store
            .create_session_with_id_and_artifact_root(
                None,
                dir.path().join("cwd"),
                dir.path().join("artifacts"),
                "sess-1".to_string(),
            )
            .unwrap();
        let mut config =
            ProviderRunConfig::new(crate::config_overrides::ProviderBackend::Fake, "fake");
        config.options.browser_mode = Some("cloud".to_string());
        config.options.browser_profile_id = Some("cloud-profile-123".to_string());
        config.options.browser_profile_label = Some("Work Cloud".to_string());
        let prepared = prepare_existing_session(&store, &session.id, &mut config).unwrap();
        assert!(prepared.worker.enabled);
        let paths = SimpleHarnessPaths::from_session(store.state_dir(), &session);
        assert_eq!(
            fs::read_to_string(paths.force_cloud_marker_path).unwrap(),
            "1\n"
        );
        let prepared_event = store
            .events_for_session(&session.id)
            .unwrap()
            .into_iter()
            .find(|event| event.event_type == SIMPLE_HARNESS_PREPARED_EVENT)
            .expect("harness prepared event");
        assert_eq!(
            prepared_event
                .payload
                .pointer("/browser_config/browser_profile_id")
                .and_then(serde_json::Value::as_str),
            Some("cloud-profile-123")
        );
        assert_eq!(
            prepared_event
                .payload
                .pointer("/browser_config/browser_profile_label")
                .and_then(serde_json::Value::as_str),
            Some("Work Cloud")
        );
        let cleanup = cleanup_existing_session(&store, &session.id).unwrap();
        assert!(cleanup.worker_present);
        assert!(cleanup.worker_stopped);
    }
}
