from __future__ import annotations

import contextlib
import atexit
import base64
import fnmatch
import hashlib
import ipaddress
import importlib
import importlib.util
import io
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import traceback
import mimetypes
import re
import tempfile
import time
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any, Dict


_namespaces: Dict[str, Dict[str, Any]] = {}
_managed_chrome: subprocess.Popen[Any] | None = None
_managed_chrome_profile: Path | None = None
_managed_chrome_profile_is_temporary = False
_explicit_agent_workspace = os.environ.get("BH_AGENT_WORKSPACE")


_HINT_PATTERNS = [
    (
        re.compile(r"':contains' is not a valid (CSS )?selector"),
        "':contains' is jQuery, not CSS. Use Array.from(document.querySelectorAll(sel)).filter(el => el.textContent.includes('X')).",
    ),
    (
        re.compile(r"Identifier '[^']+' has already been declared"),
        "Browser JS execution contexts persist. Wrap repeated const/let/class declarations in an IIFE like (()=>{...})() or use var.",
    ),
    (
        re.compile(r"Blocked a frame with origin .+ from accessing a cross-origin frame"),
        "Cross-origin iframe DOM access is blocked by the browser. Use iframe_target() or target-level CDP instead.",
    ),
    (
        re.compile(r"-32602.*No target with given id found"),
        "The target closed or was replaced. Call Target.getTargets/list_tabs() and use a fresh targetId.",
    ),
    (
        re.compile(r"Runtime\.getExecutionContexts.*wasn't found"),
        "Runtime.getExecutionContexts is not a CDP method. Use Target.getTargets/list_tabs(), then attach to the target you need.",
    ),
]


class _ToolTimeoutError(TimeoutError):
    pass


def _raise_tool_timeout(signum: int, frame: Any) -> None:
    raise _ToolTimeoutError("python tool timed out")


def _jsonable(value: Any) -> Any:
    try:
        json.dumps(value)
        return value
    except TypeError:
        return repr(value)


_MISSING_STRING_VALUES = {
    "n a",
    "na",
    "nil",
    "none",
    "null",
    "not applicable",
    "tbd",
    "to be determined",
    "unknown",
}

_MISSING_STRING_PHRASES = (
    "cannot determine",
    "could not determine",
    "not accessible",
    "not available",
    "not captured",
    "not disclosed",
    "not found",
    "not listed",
    "not provided",
    "not shown",
    "not specified",
    "not visible",
    "unable to determine",
    "unavailable",
)


def _normalized_missing_text(value: str) -> str:
    text = re.sub(r"[_/\\|]+", " ", value.strip().lower())
    text = re.sub(r"[\s.,:;()[\]{}-]+", " ", text)
    return text.strip()


def _is_missing(value: Any) -> bool:
    if value is None:
        return True
    if isinstance(value, str):
        normalized = _normalized_missing_text(value)
        if not normalized:
            return True
        if normalized in _MISSING_STRING_VALUES or normalized.startswith("unknown from "):
            return True
        return any(phrase in normalized for phrase in _MISSING_STRING_PHRASES)
    if isinstance(value, (list, tuple, set, dict)):
        return len(value) == 0
    return False


def _audit_looks_computed(audit: Dict[str, Any]) -> bool:
    checks = audit.get("checks")
    return (
        audit.get("generated_by") == "audit_artifact"
        and isinstance(audit.get("record_count"), int)
        and isinstance(checks, dict)
        and "ready_for_done" in audit
    )


def _field_value(record: Any, field: str) -> Any:
    current = record
    for part in str(field).split("."):
        if isinstance(current, dict):
            current = current.get(part)
        else:
            current = getattr(current, part, None)
        if current is None:
            return None
    return current


def _dedupe_key(record: Any, fields: list[str] | None) -> str:
    if fields:
        parts = [_field_value(record, field) for field in fields]
    else:
        parts = [record]
    return json.dumps(parts, ensure_ascii=False, sort_keys=True, default=str)


def _metric_number(value: Any) -> float | None:
    if isinstance(value, bool) or value is None:
        return None
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        normalized = value.replace(",", "")
        match = re.search(r"-?\d+(?:\.\d+)?", normalized)
        if match:
            return float(match.group(0))
    return None


def _records_from_path(data: Any, record_path: str | None) -> Any:
    if not record_path:
        return data
    current = [data]
    for raw_part in record_path.split("."):
        is_many = raw_part.endswith("[]")
        part = raw_part[:-2] if is_many else raw_part
        next_values = []
        for value in current:
            if part:
                if isinstance(value, dict):
                    value = value.get(part)
                else:
                    value = getattr(value, part, None)
            if is_many:
                if isinstance(value, list):
                    next_values.extend(value)
            else:
                next_values.append(value)
        current = next_values
    return current


def _load_json_or_csv(path: Path) -> Any:
    if path.suffix.lower() == ".csv":
        import csv

        with path.open(newline="", encoding="utf-8") as handle:
            return list(csv.DictReader(handle))
    return json.loads(path.read_text(encoding="utf-8"))


def _visual_file_audit(paths: list[Any]) -> list[Dict[str, Any]]:
    checks: list[Dict[str, Any]] = []
    for raw_path in paths:
        path = Path(str(raw_path)).expanduser()
        record: Dict[str, Any] = {"path": str(path), "exists": path.exists()}
        if path.exists() and path.is_file():
            record["bytes"] = path.stat().st_size
            try:
                from PIL import Image, ImageStat

                with Image.open(path) as img:
                    rgb = img.convert("RGB")
                    small = rgb.resize((min(64, rgb.width), min(64, rgb.height)))
                    colors = small.getcolors(maxcolors=1_000_000) or []
                    stat = ImageStat.Stat(rgb)
                    record.update(
                        {
                            "width": rgb.width,
                            "height": rgb.height,
                            "sample_unique_colors": len(colors),
                            "mean_rgb": [round(value, 1) for value in stat.mean],
                            "appears_blank": len(colors) <= 1,
                        }
                    )
            except Exception as exc:
                record["image_error"] = str(exc)
        checks.append(record)
    return checks


def _browser_mode() -> str:
    return os.environ.get("LLM_BROWSER_BROWSER_MODE", "").lower().replace("_", "-").replace(" ", "-")


def _browser_user_agent() -> str | None:
    value = os.environ.get("BU_BROWSER_USER_AGENT")
    if value is None:
        return None
    value = value.strip()
    return value or None


def _apply_browser_user_agent_override(cdp: Any, session_id: Any = None) -> None:
    user_agent = _browser_user_agent()
    if not user_agent:
        return
    with contextlib.suppress(Exception):
        cdp("Network.setUserAgentOverride", session_id=session_id, userAgent=user_agent)


def _browser_permissions() -> list[str]:
    raw = os.environ.get("BU_BROWSER_PERMISSIONS")
    if not raw:
        return []
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError:
        return []
    if not isinstance(parsed, list):
        return []
    permissions: list[str] = []
    seen: set[str] = set()
    for permission in parsed:
        if not isinstance(permission, str):
            continue
        permission = permission.strip()
        if not permission or permission in seen:
            continue
        permissions.append(permission)
        seen.add(permission)
    return permissions


def _apply_browser_permissions(cdp: Any) -> None:
    permissions = _browser_permissions()
    if not permissions:
        return
    with contextlib.suppress(Exception):
        cdp("Browser.grantPermissions", permissions=permissions)


def _browser_download_behavior() -> dict[str, Any] | None:
    accept_downloads = _env_bool("BU_BROWSER_ACCEPT_DOWNLOADS")
    if accept_downloads is False:
        return {"behavior": "deny"}

    raw_path = os.environ.get("BU_BROWSER_DOWNLOADS_PATH")
    if not raw_path or not raw_path.strip():
        return None
    download_path = Path(raw_path.strip()).expanduser().resolve()
    with contextlib.suppress(OSError):
        download_path.mkdir(parents=True, exist_ok=True)
    return {"behavior": "allow", "downloadPath": str(download_path), "eventsEnabled": True}


def _apply_browser_download_behavior(cdp: Any) -> None:
    behavior = _browser_download_behavior()
    if not behavior:
        return
    with contextlib.suppress(Exception):
        cdp("Browser.setDownloadBehavior", **behavior)


def _browser_storage_state_raw() -> str | None:
    raw = os.environ.get("BU_BROWSER_STORAGE_STATE")
    if not raw or not raw.strip():
        return None
    return raw


def _browser_storage_state() -> dict[str, Any] | None:
    raw = _browser_storage_state_raw()
    if raw is None:
        return None
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError:
        return None
    return parsed if isinstance(parsed, dict) else None


def _browser_storage_cookies(storage_state: dict[str, Any]) -> list[dict[str, Any]]:
    raw_cookies = storage_state.get("cookies")
    if not isinstance(raw_cookies, list):
        return []
    cookies: list[dict[str, Any]] = []
    for cookie in raw_cookies:
        if not isinstance(cookie, dict):
            continue
        if not isinstance(cookie.get("name"), str) or not isinstance(cookie.get("value"), str):
            continue
        cookies.append(cookie)
    return cookies


def _browser_storage_init_scripts(storage_state: dict[str, Any]) -> list[str]:
    origins = storage_state.get("origins")
    if not isinstance(origins, list):
        return []
    scripts: list[str] = []
    for origin_state in origins:
        if not isinstance(origin_state, dict):
            continue
        origin = origin_state.get("origin")
        statements: list[str] = []
        for storage_name in ("localStorage", "sessionStorage"):
            items = origin_state.get(storage_name)
            if not isinstance(items, list):
                continue
            for item in items:
                if not isinstance(item, dict):
                    continue
                name = item.get("name")
                value = item.get("value")
                if not isinstance(name, str) or not isinstance(value, str):
                    continue
                statements.append(
                    f"window.{storage_name}.setItem({json.dumps(name)}, {json.dumps(value)});"
                )
        if not statements:
            continue
        body = "\n".join(statements)
        if isinstance(origin, str) and origin:
            scripts.append(
                "try {\n"
                f"  if (window.location.origin === {json.dumps(origin)}) {{\n"
                f"    {body}\n"
                "  }\n"
                "} catch (error) {}"
            )
        else:
            scripts.append(f"try {{\n  {body}\n}} catch (error) {{}}")
    return scripts


def _apply_browser_storage_state(
    cdp: Any,
    session_id: Any = None,
    applied: set[tuple[Any, ...]] | None = None,
) -> None:
    raw = _browser_storage_state_raw()
    if raw is None:
        return
    storage_state = _browser_storage_state()
    if not storage_state:
        return

    signature = hashlib.sha256(raw.encode("utf-8")).hexdigest()
    cookies = _browser_storage_cookies(storage_state)
    cookie_key = ("storage_cookies", signature)
    if cookies and (applied is None or cookie_key not in applied):
        try:
            cdp("Storage.setCookies", session_id=session_id, cookies=cookies)
        except Exception:
            pass
        else:
            if applied is not None:
                applied.add(cookie_key)

    if session_id is None:
        return
    for index, script in enumerate(_browser_storage_init_scripts(storage_state)):
        script_key = ("storage_script", str(session_id), signature, index)
        if applied is not None and script_key in applied:
            continue
        try:
            cdp(
                "Page.addScriptToEvaluateOnNewDocument",
                session_id=session_id,
                source=script,
                runImmediately=True,
            )
        except Exception:
            pass
        else:
            if applied is not None:
                applied.add(script_key)


def _env_json_string_list(name: str) -> list[str]:
    raw = os.environ.get(name)
    if not raw:
        return []
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError:
        return []
    if not isinstance(parsed, list):
        return []
    return [value.strip() for value in parsed if isinstance(value, str) and value.strip()]


def _is_root_domain(domain: str) -> bool:
    if "*" in domain or "://" in domain:
        return False
    return domain.count(".") == 1


def _is_ip_address(host: str) -> bool:
    with contextlib.suppress(ValueError):
        ipaddress.ip_address(host)
        return True
    return False


def _domain_pattern_matches(url: str, host: str, scheme: str, pattern: str) -> bool:
    parsed_url = urllib.parse.urlparse(url)
    full_url_pattern = f"{scheme}://{host}"
    pattern = pattern.strip()
    if not pattern:
        return False
    if "*" in pattern:
        if pattern.startswith("*."):
            domain_part = pattern[2:].lower()
            host_lower = host.lower()
            return scheme in {"http", "https"} and (
                host_lower == domain_part or host_lower.endswith(f".{domain_part}")
            )
        if pattern.endswith("/*"):
            prefix = pattern[:-1]
            if "://" in prefix:
                parsed_pattern = urllib.parse.urlparse(prefix)
                pattern_host = parsed_pattern.hostname
                if not pattern_host:
                    return False
                return (
                    parsed_pattern.scheme.lower() == scheme.lower()
                    and pattern_host.lower() == host.lower()
                    and parsed_url.path.startswith(parsed_pattern.path or "/")
                )
            return url.startswith(pattern[:-1])
        return fnmatch.fnmatch(full_url_pattern if "://" in pattern else host, pattern)
    if "://" in pattern:
        parsed_pattern = urllib.parse.urlparse(pattern)
        pattern_host = parsed_pattern.hostname
        if not pattern_host:
            return False
        if parsed_pattern.scheme.lower() != scheme.lower() or pattern_host.lower() != host.lower():
            return False
        pattern_path = parsed_pattern.path or ""
        if pattern_path and pattern_path != "/" and not parsed_url.path.startswith(pattern_path):
            return False
        return True
    host_lower = host.lower()
    pattern_lower = pattern.lower()
    if host_lower == pattern_lower:
        return True
    return _is_root_domain(pattern_lower) and host_lower == f"www.{pattern_lower}"


def _browser_profile_url_allowed(url: str) -> bool:
    if url in {"about:blank", "chrome://new-tab-page/", "chrome://new-tab-page", "chrome://newtab/"}:
        return True
    try:
        parsed = urllib.parse.urlparse(url)
    except Exception:
        return False
    if parsed.scheme in {"data", "blob"}:
        return True
    host = parsed.hostname
    if not host:
        return False
    if _env_bool("BU_BROWSER_BLOCK_IP_ADDRESSES") is True and _is_ip_address(host):
        return False

    allowed_domains = _env_json_string_list("BU_BROWSER_ALLOWED_DOMAINS")
    prohibited_domains = _env_json_string_list("BU_BROWSER_PROHIBITED_DOMAINS")
    if prohibited_domains and any(
        _domain_pattern_matches(url, host, parsed.scheme, pattern) for pattern in prohibited_domains
    ):
        return False
    if allowed_domains:
        return any(_domain_pattern_matches(url, host, parsed.scheme, pattern) for pattern in allowed_domains)
    return True


def _enforce_browser_domain_constraints(method: str, params: dict[str, Any]) -> None:
    url = params.get("url")
    if not isinstance(url, str) or not url:
        return
    if method not in {"Page.navigate", "Target.createTarget"} and "." in method:
        return
    if not _browser_profile_url_allowed(url):
        raise RuntimeError(f"BrowserProfile domain constraints blocked navigation to {url}")


def _annotate_error(msg: str) -> str:
    for pattern, hint in _HINT_PATTERNS:
        if pattern.search(msg):
            return f"{msg}\nHint: {hint}"
    return msg


def _agent_workspace_path(cwd: Path) -> Path:
    explicit_agent_workspace = os.environ.get("BH_AGENT_WORKSPACE") or _explicit_agent_workspace
    if explicit_agent_workspace:
        return Path(explicit_agent_workspace).expanduser()
    home = Path(os.environ.get("BUT_HOME") or Path.home() / ".browser-use-terminal")
    return (home / "agent-workspace").expanduser()


def _outputs_dir_path(cwd: Path) -> Path:
    return cwd


def _pick_chromium_path() -> str:
    if path := os.environ.get("CHROME_PATH"):
        return path

    # Do not attach automated tests to the user's personal Chrome profile.
    # Recent Chrome builds show a blocking "Allow remote debugging?" prompt
    # for CDP. Homebrew/system Chromium builds can also be quarantined or
    # otherwise blocked by macOS Gatekeeper, so managed browser mode defaults
    # to Playwright's bundled testing browser and requires explicit opt-in for
    # system browsers.
    allow_system_chromium = os.environ.get("LLM_BROWSER_ALLOW_SYSTEM_CHROMIUM") == "1"
    allow_google_chrome = os.environ.get("LLM_BROWSER_ALLOW_GOOGLE_CHROME") == "1"
    candidates = _playwright_chromium_candidates()
    if allow_system_chromium:
        candidates.extend(
            [
                "/opt/homebrew/Caskroom/chromium/latest/chrome-mac/Chromium.app/Contents/MacOS/Chromium",
                "/Applications/Chromium.app/Contents/MacOS/Chromium",
                "/opt/homebrew/bin/chromium",
                "/usr/local/bin/chromium",
                "chromium",
                "chromium-browser",
            ]
        )
    if allow_google_chrome:
        candidates.extend(
            [
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                "google-chrome",
                "google-chrome-stable",
            ]
        )
    for candidate in candidates:
        path = Path(candidate)
        if path.is_absolute():
            resolved = path
        else:
            found = shutil.which(candidate)
            if not found:
                continue
            resolved = Path(found)
        if not resolved.exists():
            continue
        if not allow_google_chrome and _looks_like_google_chrome_wrapper(resolved):
            continue
        return str(resolved)
    raise RuntimeError(
        "Playwright Chromium not found; run `python -m playwright install chromium`, "
        "set CHROME_PATH explicitly, use Browser Use cloud, or opt into system Chromium "
        "with LLM_BROWSER_ALLOW_SYSTEM_CHROMIUM=1"
    )


def _playwright_chromium_candidates() -> list[str]:
    roots = [
        Path.home() / "Library/Caches/ms-playwright",
        Path.home() / ".cache/ms-playwright",
    ]
    matches: list[Path] = []
    for root in roots:
        if not root.exists():
            continue
        matches.extend(
            root.glob("chromium-*/chrome-mac*/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing")
        )
    return [str(path) for path in sorted(matches, reverse=True)]


def _looks_like_google_chrome_wrapper(path: Path) -> bool:
    try:
        if not path.is_file() or path.stat().st_size > 4096:
            return False
        return "Google Chrome.app" in path.read_text(errors="ignore")
    except OSError:
        return False


def _free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def _managed_chrome_is_visible() -> bool:
    mode = _browser_mode()
    if mode in {"managed-headed", "headed", "headful"}:
        return True
    if mode in {"managed-headless", "headless", "headless-chromium"}:
        return False
    return os.environ.get("LLM_BROWSER_MANAGED_CHROME_VISIBLE") == "1"


def _should_start_managed_chrome() -> bool:
    if os.environ.get("BU_CDP_URL") or os.environ.get("BU_CDP_WS") or os.environ.get("BU_BROWSER_ID"):
        return False
    return (
        _browser_mode()
        in {"managed-headless", "managed-headed", "headless", "headless-chromium", "headed", "headful"}
        or os.environ.get("LLM_BROWSER_AUTO_CHROME") == "1"
    )


def _pick_managed_chrome_path(visible: bool) -> str:
    if not visible:
        return _pick_chromium_path()
    if path := os.environ.get("CHROME_PATH"):
        return path
    google_chrome = Path("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
    if google_chrome.exists():
        return str(google_chrome)
    return _pick_chromium_path()


def _managed_chrome_extra_args() -> list[str]:
    raw = os.environ.get("BU_MANAGED_BROWSER_ARGS")
    if not raw:
        return []
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError:
        return []
    if not isinstance(parsed, list):
        return []
    return [arg for arg in parsed if isinstance(arg, str) and arg]


def _managed_chrome_profile_dir() -> tuple[Path, bool]:
    configured = os.environ.get("BU_MANAGED_BROWSER_PROFILE")
    if configured and configured.strip():
        profile = Path(configured).expanduser()
        profile.mkdir(parents=True, exist_ok=True)
        return profile, False
    return Path(tempfile.mkdtemp(prefix="but-managed-chrome.")), True


def _env_bool(name: str) -> bool | None:
    raw = os.environ.get(name)
    if raw is None:
        return None
    value = raw.strip().lower()
    if value in {"1", "true", "yes", "on"}:
        return True
    if value in {"0", "false", "no", "off"}:
        return False
    return None


def _env_milliseconds_to_seconds(name: str) -> float:
    raw = os.environ.get(name)
    if raw is None:
        return 0.0
    try:
        milliseconds = float(raw.strip())
    except ValueError:
        return 0.0
    if milliseconds <= 0:
        return 0.0
    return milliseconds / 1000.0


def _apply_browser_wait_between_actions() -> None:
    wait_seconds = _env_milliseconds_to_seconds("BU_BROWSER_WAIT_BETWEEN_ACTIONS_MS")
    if wait_seconds > 0:
        time.sleep(wait_seconds)


def _managed_chrome_viewport_args() -> list[str]:
    if _env_bool("BU_BROWSER_NO_VIEWPORT") is True:
        return []
    raw = os.environ.get("BU_BROWSER_VIEWPORT")
    if not raw:
        return []
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError:
        return []
    if not isinstance(parsed, dict):
        return []
    width = parsed.get("width")
    height = parsed.get("height")
    if type(width) is not int or type(height) is not int or width <= 0 or height <= 0:
        return []
    args = [f"--window-size={width},{height}"]
    device_scale_factor = parsed.get("deviceScaleFactor")
    if isinstance(device_scale_factor, (int, float)) and device_scale_factor > 0:
        args.append(f"--force-device-scale-factor={device_scale_factor:g}")
    return args


def _managed_chrome_args(chrome: str, port: int, profile: Path, visible: bool) -> list[str]:
    viewport_args = _managed_chrome_viewport_args()
    args = [
        chrome,
        "--remote-debugging-address=127.0.0.1",
        f"--remote-debugging-port={port}",
        f"--user-data-dir={profile}",
        "--no-first-run",
        "--no-default-browser-check",
    ]
    if visible:
        args.append("--new-window")
        if not viewport_args:
            args.append("--window-size=1512,900")
    else:
        args.append("--headless=new")
    args.extend(viewport_args)
    args.extend(_managed_chrome_extra_args())
    args.append("about:blank")
    return args


def _daemon_has_browser_connection(admin: Any) -> bool:
    try:
        name = os.environ.get("BU_NAME", "default")
        connection = getattr(admin, "_daemon_browser_connection", lambda _name: None)(name)
        return bool(connection and connection.get("page"))
    except Exception:
        return False


def _cleanup_managed_chrome() -> None:
    global _managed_chrome, _managed_chrome_profile, _managed_chrome_profile_is_temporary
    proc = _managed_chrome
    _managed_chrome = None
    if proc is not None and proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)
    if _managed_chrome_profile is not None and _managed_chrome_profile_is_temporary:
        shutil.rmtree(_managed_chrome_profile, ignore_errors=True)
    _managed_chrome_profile = None
    _managed_chrome_profile_is_temporary = False


def _ensure_managed_chrome(admin: Any | None = None) -> None:
    global _managed_chrome, _managed_chrome_profile, _managed_chrome_profile_is_temporary
    if not _should_start_managed_chrome():
        return
    if admin is not None and _daemon_has_browser_connection(admin):
        return
    if _managed_chrome is not None and _managed_chrome.poll() is None:
        return

    port = _free_port()
    profile, profile_is_temporary = _managed_chrome_profile_dir()
    visible = _managed_chrome_is_visible()
    chrome = _pick_managed_chrome_path(visible)
    proc = subprocess.Popen(
        _managed_chrome_args(chrome, port, profile, visible),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=visible,
    )
    deadline = time.time() + 20
    last_error: Exception | None = None
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError("managed Chrome exited before DevTools became available")
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version", timeout=0.5).read()
            break
        except Exception as exc:  # pragma: no cover - timing/environment dependent
            last_error = exc
            time.sleep(0.25)
    else:
        proc.terminate()
        if profile_is_temporary:
            shutil.rmtree(profile, ignore_errors=True)
        raise RuntimeError(f"managed Chrome DevTools did not become available: {last_error}")

    _managed_chrome = proc
    _managed_chrome_profile = profile
    _managed_chrome_profile_is_temporary = profile_is_temporary
    os.environ["BU_CDP_URL"] = f"http://127.0.0.1:{port}"
    if not visible:
        atexit.register(_cleanup_managed_chrome)


def _ensure_cloud_browser(admin: Any) -> None:
    if _browser_mode() != "cloud":
        return
    _clear_cloud_cdp_overrides()
    if not os.environ.get("BROWSER_USE_API_KEY"):
        raise RuntimeError("Browser Use cloud selected, but BROWSER_USE_API_KEY is not set")
    name = os.environ.get("BU_NAME", "default")
    if admin.daemon_alive(name):
        if (
            os.environ.get("LLM_BROWSER_OWN_REMOTE_DAEMON") == "1"
            and os.environ.get("LLM_BROWSER_OWN_REMOTE_DAEMON_NAME") == name
        ):
            return
        if _daemon_log_is_cloud(admin, name):
            return
        with contextlib.suppress(Exception):
            admin.stop_remote_daemon(name)
        if admin.daemon_alive(name):
            raise RuntimeError(f"Browser Use cloud selected, but daemon {name!r} is not cloud-backed")
    if os.environ.get("LLM_BROWSER_OPEN_CLOUD_LIVE_VIEW") != "1":
        admin._show_live_url = lambda url: None
    browser = admin.start_remote_daemon(name=name)
    os.environ["LLM_BROWSER_OWN_REMOTE_DAEMON"] = "1"
    os.environ["LLM_BROWSER_OWN_REMOTE_DAEMON_NAME"] = name
    live_url = browser.get("liveUrl")
    if live_url:
        os.environ["LLM_BROWSER_LIVE_URL"] = str(live_url)


def _clear_cloud_cdp_overrides() -> None:
    for key in ("BU_CDP_URL", "BU_CDP_WS", "BU_BROWSER_ID"):
        value = os.environ.get(key)
        if not value:
            continue
        if key == "BU_BROWSER_ID" or _looks_like_local_cdp(value):
            os.environ.pop(key, None)
            continue
        if key in {"BU_CDP_URL", "BU_CDP_WS"} and "browser-use.com" not in value:
            raise RuntimeError(
                f"Browser Use cloud selected, but {key} points outside Browser Use cloud: {value}"
            )


def _looks_like_local_cdp(value: str) -> bool:
    lower = value.lower()
    return (
        "127.0.0.1" in lower
        or "localhost" in lower
        or lower.startswith("http://[::1]")
        or lower.startswith("ws://[::1]")
    )


def _daemon_log_is_cloud(admin: Any, name: str) -> bool:
    try:
        log_path = admin.ipc.log_path(name)
        text = Path(str(log_path)).read_text(errors="ignore")
    except Exception:
        return False
    if "remote=local" in text or "ws://127.0.0.1" in text or "ws://localhost" in text:
        return False
    return "browser-use.com" in text or ("remote=" in text and "remote=local" not in text)


def _shutdown_owned_cloud_browser() -> Dict[str, Any]:
    if _browser_mode() != "cloud" or os.environ.get("LLM_BROWSER_OWN_REMOTE_DAEMON") != "1":
        return {"stopped": False, "reason": "not_owned"}
    name = os.environ.get("LLM_BROWSER_OWN_REMOTE_DAEMON_NAME") or os.environ.get("BU_NAME", "default")
    try:
        admin = importlib.import_module("browser_harness.admin")
        admin.stop_remote_daemon(name)
    except Exception as exc:
        return {"stopped": False, "name": name, "error": str(exc)}
    os.environ.pop("LLM_BROWSER_OWN_REMOTE_DAEMON", None)
    os.environ.pop("LLM_BROWSER_OWN_REMOTE_DAEMON_NAME", None)
    return {"stopped": True, "name": name}


def _load_browser_harness(ns: Dict[str, Any]) -> None:
    if ns.get("__browser_harness_checked__"):
        return
    ns["__browser_harness_checked__"] = True
    try:
        cwd = Path(str(ns.get("cwd") or ".")).expanduser()
        os.environ.setdefault("BH_AGENT_WORKSPACE", str(_agent_workspace_path(cwd)))
        admin = importlib.import_module("browser_harness.admin")
        _patch_browser_harness_admin(admin)
        _ensure_managed_chrome(admin)
        _ensure_cloud_browser(admin)
        helpers = importlib.import_module("browser_harness.helpers")
        _patch_browser_harness_cdp(helpers, admin)
        names = getattr(helpers, "__all__", None) or [name for name in dir(helpers) if not name.startswith("_")]
        ns.update({name: getattr(helpers, name) for name in names})
        def ensure_browser_connection() -> Any:
            if _browser_mode() == "cloud":
                return _ensure_cloud_browser(admin)
            return admin.ensure_daemon()

        ns["ensure_browser_connection"] = ensure_browser_connection
        ns["browser_daemon_alive"] = admin.daemon_alive
        ns["__browser_harness_helpers__"] = helpers
        ns["__browser_harness_admin__"] = admin
        ns["browser_harness_available"] = True
        ns["browser_harness_error"] = None
    except Exception as exc:  # pragma: no cover - environment dependent
        ns["browser_harness_available"] = False
        ns["browser_harness_error"] = str(exc)


def _patch_browser_harness_cdp(helpers: Any, admin: Any) -> None:
    if getattr(helpers, "__llm_browser_cdp_patched__", False):
        return
    original_cdp = helpers.cdp
    applied_browser_profile_state: set[tuple[Any, ...]] = set()

    def cdp_with_daemon(method: str, session_id: Any = None, **params: Any) -> Any:
        if _browser_mode() == "cloud":
            _ensure_cloud_browser(admin)
        else:
            admin.ensure_daemon()
        _enforce_browser_domain_constraints(method, params)
        if method != "Browser.grantPermissions":
            _apply_browser_permissions(original_cdp)
        if method != "Browser.setDownloadBehavior":
            _apply_browser_download_behavior(original_cdp)
        if method not in {"Storage.setCookies", "Page.addScriptToEvaluateOnNewDocument"}:
            _apply_browser_storage_state(
                original_cdp,
                session_id=session_id,
                applied=applied_browser_profile_state,
            )
        if method != "Network.setUserAgentOverride":
            _apply_browser_user_agent_override(original_cdp, session_id=session_id)
        return original_cdp(method, session_id=session_id, **params)

    helpers.__llm_browser_original_cdp__ = original_cdp
    helpers.__llm_browser_cdp_patched__ = True
    helpers.cdp = cdp_with_daemon


def _patch_browser_harness_admin(admin: Any) -> None:
    if getattr(admin, "__llm_browser_admin_patched__", False):
        return
    original_ensure_daemon = admin.ensure_daemon

    def ensure_daemon_cloud_guard(*args: Any, **kwargs: Any) -> Any:
        env = kwargs.get("env")
        if env is None and len(args) >= 3:
            env = args[2]
        if _browser_mode() == "cloud" and not (isinstance(env, dict) and env.get("BU_CDP_WS")):
            name = kwargs.get("name") or os.environ.get("BU_NAME", "default")
            _ensure_cloud_browser(admin)
            if not _daemon_log_is_cloud(admin, str(name)):
                raise RuntimeError(
                    f"Browser Use cloud selected, but daemon {name!r} is not cloud-backed"
                )
            return None
        return original_ensure_daemon(*args, **kwargs)

    admin.__llm_browser_original_ensure_daemon__ = original_ensure_daemon
    admin.__llm_browser_admin_patched__ = True
    admin.ensure_daemon = ensure_daemon_cloud_guard


def _emit_protocol_event(request_id: str, event: str, payload: Dict[str, Any]) -> None:
    print(
        json.dumps(
            {
                "id": request_id,
                "event": event,
                "payload": _jsonable(payload),
            },
            ensure_ascii=False,
            separators=(",", ":"),
        ),
        file=sys.__stdout__,
        flush=True,
    )


def _record_browser_event(
    ns: Dict[str, Any],
    request_id: str,
    event_type: str,
    payload: Dict[str, Any],
) -> Dict[str, Any]:
    record = {"type": event_type, "payload": payload}
    ns.setdefault("browser_events", []).append(record)
    _emit_protocol_event(request_id, "browser", record)
    return record


def _namespace(session_id: str, cwd: Path, artifact_dir: Path) -> Dict[str, Any]:
    ns = _namespaces.setdefault(
        session_id,
        {
            "__name__": "__browser_use_worker__",
            "session_id": session_id,
        },
    )
    artifact_dir.mkdir(parents=True, exist_ok=True)
    _outputs_dir_path(cwd).mkdir(parents=True, exist_ok=True)
    workspace = _agent_workspace_path(cwd)
    workspace.mkdir(parents=True, exist_ok=True)
    (workspace / "agent_helpers.py").touch(exist_ok=True)
    ns["cwd"] = cwd
    ns["artifact_dir"] = artifact_dir
    ns["result"] = None
    ns["images"] = []
    ns["outputs"] = []
    ns["artifacts"] = []
    ns["browser_events"] = []
    _load_browser_harness(ns)
    return ns


def _safe_name(name: str) -> str:
    return "".join(ch if ch.isalnum() or ch in "._-" else "_" for ch in name).strip("._") or "artifact"


def _load_agent_helpers_into_ns(
    ns: Dict[str, Any],
    path: Any | None = None,
    force: bool = False,
) -> Dict[str, Any]:
    cwd = Path(str(ns.get("cwd") or ".")).expanduser()
    workspace = _agent_workspace_path(cwd)
    helper_path = Path(str(path)).expanduser() if path is not None else workspace / "agent_helpers.py"
    if not helper_path.is_absolute():
        helper_path = cwd / helper_path
    if not helper_path.exists():
        return {"loaded": False, "path": str(helper_path), "reason": "missing"}
    stat = helper_path.stat()
    cache_key = str(helper_path.resolve())
    stamp = (stat.st_mtime_ns, stat.st_size)
    loaded_cache = ns.setdefault("__agent_helpers_loaded__", {})
    if not force and loaded_cache.get(cache_key) == stamp:
        return {"loaded": False, "path": str(helper_path), "reason": "unchanged"}
    spec = importlib.util.spec_from_file_location(
        f"browser_use_agent_helpers_{_safe_name(str(ns.get('session_id') or 'default'))}",
        helper_path,
    )
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load agent helper module from {helper_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    names = []
    for name, value in vars(module).items():
        if name.startswith("_"):
            continue
        ns[name] = value
        names.append(name)
    loaded_cache[cache_key] = stamp
    return {"loaded": True, "path": str(helper_path), "names": sorted(names)}


def _install_host_helpers(ns: Dict[str, Any], request_id: str, cancel_requested: bool) -> None:
    artifact_dir = Path(ns["artifact_dir"])
    if "__browser_harness_capture_screenshot__" not in ns:
        ns["__browser_harness_capture_screenshot__"] = ns.get("capture_screenshot")
    if "__browser_harness_page_info__" not in ns:
        ns["__browser_harness_page_info__"] = ns.get("page_info")
    if "__browser_harness_cdp__" not in ns:
        ns["__browser_harness_cdp__"] = ns.get("cdp")

    def emit_output(text: Any) -> None:
        record = {"text": str(text)}
        ns.setdefault("outputs", []).append(record)
        _emit_protocol_event(request_id, "output", record)

    def _copy_artifact(
        path: Any,
        kind: str = "file",
        name: str | None = None,
        mime: str | None = None,
        emit_event: bool = True,
    ) -> Dict[str, Any]:
        source = Path(str(path)).expanduser()
        if not source.is_absolute():
            source = Path.cwd() / source
        if not source.exists() or not source.is_file():
            raise FileNotFoundError(str(source))
        target_dir = artifact_dir / ("images" if kind == "image" else "files")
        target_dir.mkdir(parents=True, exist_ok=True)
        target_name = _safe_name(name or source.name)
        target = target_dir / target_name
        if target.exists():
            stem = target.stem
            suffix = target.suffix
            idx = 2
            while target.exists():
                target = target_dir / f"{stem}-{idx}{suffix}"
                idx += 1
        shutil.copy2(source, target)
        record = {
            "kind": kind,
            "path": str(target),
            "source_path": str(source),
            "mime": mime or mimetypes.guess_type(str(target))[0],
            "bytes": target.stat().st_size,
        }
        ns.setdefault("artifacts", []).append(record)
        if emit_event:
            _emit_protocol_event(request_id, "artifact", record)
        return record

    def copy_artifact(path: Any, kind: str = "file", name: str | None = None, mime: str | None = None) -> Dict[str, Any]:
        return _copy_artifact(path, kind=kind, name=name, mime=mime, emit_event=True)

    def _write_artifact(name: str, content: str, mime: str) -> Dict[str, Any]:
        target_dir = artifact_dir / "files"
        target_dir.mkdir(parents=True, exist_ok=True)
        target_name = _safe_name(name)
        target = target_dir / target_name
        if target.exists():
            stem = target.stem
            suffix = target.suffix
            idx = 2
            while target.exists():
                target = target_dir / f"{stem}-{idx}{suffix}"
                idx += 1
        target.write_text(content, encoding="utf-8")
        record = {
            "kind": "file",
            "path": str(target),
            "source_path": str(target),
            "mime": mime,
            "bytes": target.stat().st_size,
        }
        ns.setdefault("artifacts", []).append(record)
        _emit_protocol_event(request_id, "artifact", record)
        return record

    def emit_image(path: Any, label: str | None = None, detail: str = "auto", mime_type: str | None = None) -> Dict[str, Any]:
        record = _copy_artifact(path, kind="image", mime=mime_type, emit_event=False)
        image = {
            "label": label,
            "path": record["path"],
            "mime_type": record.get("mime") or "image/png",
            "detail": detail,
        }
        ns.setdefault("images", []).append(image)
        _emit_protocol_event(request_id, "image", image)
        return image

    def _cdp_screenshot_extension(params: Dict[str, Any]) -> str:
        fmt = str(params.get("format") or "png").lower()
        if fmt in {"jpeg", "jpg"}:
            return ".jpg"
        if fmt == "webp":
            return ".webp"
        return ".png"

    def _attach_cdp_screenshot(result: Any, params: Dict[str, Any]) -> None:
        if ns.get("__suppress_cdp_screenshot_attach__"):
            return
        if not isinstance(result, dict) or not result.get("data"):
            return
        raw_dir = artifact_dir / ".raw-cdp-screenshots"
        raw_dir.mkdir(parents=True, exist_ok=True)
        idx = int(ns.get("__raw_cdp_screenshot_counter__", 0)) + 1
        ns["__raw_cdp_screenshot_counter__"] = idx
        path = raw_dir / f"cdp-screenshot-{idx}{_cdp_screenshot_extension(params)}"
        path.write_bytes(base64.b64decode(str(result["data"])))
        emit_image(
            path,
            label=f"cdp_screenshot_{idx}",
            detail="auto",
            mime_type=mimetypes.guess_type(str(path))[0] or "image/png",
        )

    @contextlib.contextmanager
    def _suppress_cdp_screenshot_attach():
        previous = ns.get("__suppress_cdp_screenshot_attach__")
        ns["__suppress_cdp_screenshot_attach__"] = True
        try:
            yield
        finally:
            if previous is None:
                ns.pop("__suppress_cdp_screenshot_attach__", None)
            else:
                ns["__suppress_cdp_screenshot_attach__"] = previous

    def cdp(method: str, session_id: Any = None, **params: Any) -> Any:
        original = ns.get("__browser_harness_cdp__")
        if not callable(original):
            raise RuntimeError("browser_harness cdp is not available")
        result = original(method, session_id=session_id, **params)
        if method == "Page.captureScreenshot":
            _attach_cdp_screenshot(result, params)
        return result

    def _screenshot_label(label: str | None = None, path: Any | None = None) -> str:
        if label:
            return str(label)
        if path:
            stem = Path(str(path)).expanduser().stem
            if stem:
                return stem
        return "screenshot"

    def capture_screenshot(
        path: Any | None = None,
        full: bool = False,
        max_dim: int | None = None,
        attach: bool = True,
        label: str | None = None,
        detail: str = "auto",
        mime_type: str | None = None,
        timeout: float | None = None,
        timeout_s: float | None = None,
    ) -> str:
        """Browser-harness capture_screenshot plus direct image attachment.

        Browser-harness itself returns a PNG path. The Rust browser agent keeps
        that contract while defaulting to attach=True so the next model turn
        receives the pixels directly in the tool result.
        """
        _ = timeout, timeout_s  # Accepted for compatibility with the main Python implementation.
        original = ns.get("__browser_harness_capture_screenshot__")
        if not callable(original):
            raise RuntimeError("browser_harness capture_screenshot is not available")
        shot_path = original(path=path, full=full, max_dim=max_dim)
        if attach:
            emit_image(
                shot_path,
                label=_screenshot_label(label=label, path=path),
                detail=detail,
                mime_type=mime_type,
            )
        return str(shot_path)

    def _apply_max_dim(path: Any, max_dim: int | None = None) -> None:
        if not max_dim:
            return
        from PIL import Image

        img = Image.open(path)
        if max(img.size) > max_dim:
            img.thumbnail((max_dim, max_dim))
            img.save(path)

    def screenshot_clip(
        label: str = "screenshot_clip",
        x: float = 0,
        y: float = 0,
        width: float = 1,
        height: float = 1,
        path: Any | None = None,
        scale: float = 1,
        capture_beyond_viewport: bool = False,
        max_dim: int | None = None,
        attach: bool = True,
        detail: str = "auto",
        mime_type: str | None = None,
    ) -> Dict[str, Any] | str:
        """Capture a CSS-pixel viewport/page clip via CDP and optionally attach it.

        This is intentionally CDP-native instead of post-cropping a full PNG.
        Coordinates are CSS pixels, matching `click_at_xy`, not device pixels.
        """
        cdp_fn = ns.get("cdp")
        if not callable(cdp_fn):
            raise RuntimeError("browser_harness cdp is not available")
        if width <= 0 or height <= 0:
            raise ValueError("screenshot_clip width and height must be positive")
        shot_path = Path(str(path)).expanduser() if path is not None else artifact_dir / f"{_safe_name(label)}.png"
        if not shot_path.is_absolute():
            shot_path = Path.cwd() / shot_path
        shot_path.parent.mkdir(parents=True, exist_ok=True)
        with _suppress_cdp_screenshot_attach():
            response = cdp_fn(
                "Page.captureScreenshot",
                format="png",
                captureBeyondViewport=bool(capture_beyond_viewport),
                clip={
                    "x": float(x),
                    "y": float(y),
                    "width": float(width),
                    "height": float(height),
                    "scale": float(scale),
                },
            )
        shot_path.write_bytes(base64.b64decode(response["data"]))
        _apply_max_dim(shot_path, max_dim=max_dim)
        if not attach:
            return str(shot_path)
        return emit_image(
            shot_path,
            label=label,
            detail=detail,
            mime_type=mime_type,
        )

    def screenshot(
        label: str = "screenshot",
        path: Any | None = None,
        full: bool = False,
        max_dim: int | None = None,
        attach: bool = True,
        detail: str = "auto",
        mime_type: str | None = None,
        timeout: float | None = None,
        timeout_s: float | None = None,
    ) -> Dict[str, Any] | str:
        """Capture and attach a screenshot in one call."""
        shot_path = capture_screenshot(
            path=path,
            full=full,
            max_dim=max_dim,
            attach=False,
            timeout=timeout,
            timeout_s=timeout_s,
        )
        if not attach:
            return shot_path
        return emit_image(
            shot_path,
            label=label,
            detail=detail,
            mime_type=mime_type,
        )

    def page_info() -> Dict[str, Any]:
        """Browser-harness page_info with a CDP-only fallback for wedged page JS."""
        original = ns.get("__browser_harness_page_info__")
        if callable(original):
            try:
                return original()
            except Exception as exc:
                fallback = _page_info_cdp_fallback(ns, exc)
                if fallback:
                    return fallback
                raise
        fallback = _page_info_cdp_fallback(ns, None)
        if fallback:
            return fallback
        raise RuntimeError("browser_harness page_info is not available")

    def _final_answer_count(data: Any) -> int | None:
        if isinstance(data, list):
            return len(data)
        if isinstance(data, dict):
            list_lengths = [len(value) for value in data.values() if isinstance(value, list)]
            if list_lengths:
                return sum(list_lengths)
        return None

    def _final_answer_preview(data: Any) -> Any:
        if isinstance(data, list):
            return data[:3]
        if isinstance(data, dict):
            preview: Dict[str, Any] = {}
            for key, value in data.items():
                preview[key] = value[:3] if isinstance(value, list) else value
            return preview
        text = str(data)
        return text[:1000] + ("..." if len(text) > 1000 else "")

    def set_final_answer(
        data: Any,
        artifact_name: str | None = None,
        mime_type: str | None = None,
        audit: Dict[str, Any] | None = None,
    ) -> Dict[str, Any]:
        """Persist the final user-facing answer for a later `done` call.

        The full answer is written to `.final_answer.json` for the host and,
        when `artifact_name` is supplied, copied as a user-visible artifact.
        The Python `result` is only a compact readiness summary, avoiding huge
        transcript prints as the source of truth.
        """
        nonlocal artifact_dir
        if isinstance(data, str):
            result_text = data
            default_mime = "text/plain"
            default_name = "final_answer.txt"
        else:
            result_text = json.dumps(data, ensure_ascii=False, indent=2, default=str)
            default_mime = "application/json"
            default_name = "final_answer.json"
        artifact = None
        if artifact_name:
            artifact = _write_artifact(artifact_name, result_text, mime_type or default_mime)
        count = _final_answer_count(data)
        ready_for_done = True
        audit_note = None
        if isinstance(audit, dict):
            if not _audit_looks_computed(audit):
                ready_for_done = False
                audit_note = (
                    "attached audit does not match audit_artifact(...) output "
                    "(expected generated_by='audit_artifact', record_count, checks, and ready_for_done)"
                )
            else:
                ready_for_done = bool(audit.get("ready_for_done"))
        summary = {
            "ready": ready_for_done,
            "count": count,
            "artifact": artifact,
            "audit": audit,
            "ready_for_done": ready_for_done,
            "preview": _final_answer_preview(data),
        }
        if audit_note:
            summary["audit_note"] = audit_note
        metadata = {
            "result": result_text,
            "summary": summary,
            "artifact": artifact,
        }
        (artifact_dir / ".final_answer.json").write_text(
            json.dumps(metadata, ensure_ascii=False, indent=2, default=str),
            encoding="utf-8",
        )
        ns["final_answer"] = data
        ns["final_answer_text"] = result_text
        ns["result"] = {"final_answer": summary}
        count_text = f" count={count}" if count is not None else ""
        artifact_text = f" artifact={artifact['path']}" if artifact else ""
        audit_text = f" audit_ready_for_done={ready_for_done}" if isinstance(audit, dict) else ""
        emit_output(f"final answer ready:{count_text}{artifact_text}{audit_text}")
        return summary

    def audit_artifact(
        data: Any | None = None,
        path: Any | None = None,
        records: list[Any] | None = None,
        record_path: str | None = None,
        required_fields: list[str] | None = None,
        dedupe_fields: list[str] | None = None,
        bucket_field: str | None = None,
        bucket_targets: Dict[str, int] | None = None,
        visual_files: list[Any] | None = None,
        unique_visual_files: bool = False,
        source_evidence: Dict[str, Any] | None = None,
        required_source_fields: list[str] | None = None,
        selection_metric_field: str | None = None,
        selection_order: str = "desc",
        selection_limit: int | None = None,
        selection_pool_records: list[Any] | None = None,
        selection_key_fields: list[str] | None = None,
        allow_empty: bool = False,
        artifact_name: str = "artifact_audit.json",
    ) -> Dict[str, Any]:
        """Compute explicit pre-final checks requested by the agent."""
        source_path = None
        if records is None:
            if data is None and path is not None:
                source = Path(str(path)).expanduser()
                if not source.is_absolute():
                    source = Path.cwd() / source
                source_path = str(source)
                data = _load_json_or_csv(source)
            extracted = _records_from_path(data, record_path)
            if isinstance(extracted, list):
                records = extracted
            elif isinstance(extracted, dict):
                if isinstance(extracted.get("records"), list):
                    records = extracted["records"]
                elif isinstance(extracted.get("items"), list):
                    records = extracted["items"]
                else:
                    records = [extracted]
            elif extracted is None:
                records = []
            else:
                records = [extracted]

        audit: Dict[str, Any] = {
            "generated_by": "audit_artifact",
            "schema_version": 1,
            "ready_for_done": True,
            "source_path": source_path or (str(path) if path is not None else None),
            "record_count": len(records),
            "checks": {},
        }

        record_level_checks_requested = bool(
            required_fields
            or dedupe_fields is not None
            or bucket_field
            or bucket_targets
            or selection_metric_field
            or selection_limit is not None
            or selection_pool_records is not None
        )
        if not records and record_level_checks_requested and not allow_empty:
            audit["checks"]["record_count"] = {
                "allow_empty": False,
                "minimum": 1,
                "violation": "zero_records",
                "note": (
                    "Record-level checks were requested, but no records were audited. "
                    "Pass allow_empty=True only after proving the source genuinely has no matching records."
                ),
            }
            audit["ready_for_done"] = False

        if required_fields:
            missing: Dict[str, Any] = {}
            for field in required_fields:
                examples = []
                count = 0
                for idx, record in enumerate(records):
                    if _is_missing(_field_value(record, field)):
                        count += 1
                        if len(examples) < 5:
                            examples.append({"index": idx, "record": record})
                missing[field] = {"count": count, "examples": examples}
            audit["checks"]["missing_fields"] = missing
            if any(item["count"] for item in missing.values()):
                audit["ready_for_done"] = False

        if dedupe_fields is not None:
            seen: Dict[str, int] = {}
            duplicate_examples = []
            for idx, record in enumerate(records):
                key = _dedupe_key(record, dedupe_fields)
                if key in seen:
                    if len(duplicate_examples) < 10:
                        duplicate_examples.append(
                            {
                                "first_index": seen[key],
                                "duplicate_index": idx,
                                "key": json.loads(key),
                                "record": record,
                            }
                        )
                else:
                    seen[key] = idx
            duplicate_count = len(records) - len(seen)
            audit["checks"]["dedupe"] = {
                "fields": dedupe_fields,
                "duplicate_count": duplicate_count,
                "unique_count": len(seen),
                "examples": duplicate_examples,
            }
            if duplicate_count:
                audit["ready_for_done"] = False

        if bucket_field:
            counts: Dict[str, int] = {}
            for record in records:
                value = _field_value(record, bucket_field)
                values = value if isinstance(value, list) else [value]
                for item in values:
                    if _is_missing(item):
                        continue
                    counts[str(item)] = counts.get(str(item), 0) + 1
            check: Dict[str, Any] = {"field": bucket_field, "counts": counts}
            if bucket_targets:
                unmet = {
                    bucket: {"count": counts.get(str(bucket), 0), "target": int(target)}
                    for bucket, target in bucket_targets.items()
                    if counts.get(str(bucket), 0) < int(target)
                }
                check["targets"] = bucket_targets
                check["unmet_targets"] = unmet
                if unmet:
                    audit["ready_for_done"] = False
            audit["checks"]["buckets"] = check

        if visual_files:
            visual = _visual_file_audit(visual_files)
            audit["checks"]["visual_files"] = visual
            if any(
                (not item.get("exists")) or item.get("appears_blank") or item.get("image_error")
                for item in visual
            ):
                audit["ready_for_done"] = False
            if unique_visual_files:
                groups: Dict[str, list[str]] = {}
                for item in visual:
                    path_value = item.get("path")
                    if not path_value or not item.get("exists") or item.get("image_error"):
                        continue
                    path_obj = Path(str(path_value)).expanduser()
                    if not path_obj.is_file():
                        continue
                    digest = hashlib.sha256(path_obj.read_bytes()).hexdigest()
                    groups.setdefault(digest, []).append(str(path_obj))
                duplicate_groups = [
                    {"sha256": digest, "paths": paths}
                    for digest, paths in groups.items()
                    if len(paths) > 1
                ]
                audit["checks"]["visual_file_uniqueness"] = {
                    "duplicate_hash_group_count": len(duplicate_groups),
                    "duplicate_hash_groups": duplicate_groups[:10],
                }
                if duplicate_groups:
                    audit["ready_for_done"] = False

        if source_evidence is not None or required_source_fields:
            evidence = source_evidence if isinstance(source_evidence, dict) else {}
            missing_source: Dict[str, Any] = {}
            for field in required_source_fields or []:
                value = _field_value(evidence, field)
                if _is_missing(value):
                    missing_source[field] = {"value": value}
            audit["checks"]["source_evidence"] = {
                "required_fields": required_source_fields or [],
                "missing_fields": missing_source,
                "evidence": evidence,
            }
            if missing_source:
                audit["ready_for_done"] = False

        if selection_metric_field:
            order = str(selection_order or "desc").strip().lower()
            descending = order not in {"asc", "ascending", "low_to_high", "lowest"}
            missing_metric = []
            selected_metrics: list[tuple[int, float, Any]] = []
            for idx, record in enumerate(records):
                metric = _metric_number(_field_value(record, selection_metric_field))
                if metric is None:
                    if len(missing_metric) < 5:
                        missing_metric.append({"index": idx, "record": record})
                else:
                    selected_metrics.append((idx, metric, record))

            order_violations = []
            for left, right in zip(selected_metrics, selected_metrics[1:]):
                left_idx, left_metric, left_record = left
                right_idx, right_metric, right_record = right
                violates = right_metric > left_metric if descending else right_metric < left_metric
                if violates and len(order_violations) < 10:
                    order_violations.append(
                        {
                            "first_index": left_idx,
                            "first_metric": left_metric,
                            "first_record": left_record,
                            "next_index": right_idx,
                            "next_metric": right_metric,
                            "next_record": right_record,
                        }
                    )

            check: Dict[str, Any] = {
                "metric_field": selection_metric_field,
                "order": "desc" if descending else "asc",
                "selected_count": len(records),
                "selection_limit": selection_limit,
                "missing_metric_count": len(records) - len(selected_metrics),
                "missing_metric_examples": missing_metric,
                "order_violation_count": len(order_violations),
                "order_violation_examples": order_violations,
            }
            if missing_metric or order_violations:
                audit["ready_for_done"] = False
            if selection_limit is not None and len(records) < int(selection_limit):
                check["under_limit"] = {"count": len(records), "target": int(selection_limit)}
                audit["ready_for_done"] = False

            if selection_pool_records is not None:
                pool_metrics: list[tuple[int, float, Any]] = []
                pool_missing = 0
                for idx, record in enumerate(selection_pool_records):
                    metric = _metric_number(_field_value(record, selection_metric_field))
                    if metric is None:
                        pool_missing += 1
                    else:
                        pool_metrics.append((idx, metric, record))
                pool_metrics.sort(key=lambda item: item[1], reverse=descending)
                desired_count = int(selection_limit or len(records))
                top_pool = pool_metrics[:desired_count]
                check["candidate_pool_count"] = len(selection_pool_records)
                check["candidate_pool_metric_count"] = len(pool_metrics)
                check["candidate_pool_missing_metric_count"] = pool_missing
                check["candidate_pool_top_preview"] = [
                    {"index": idx, "metric": metric, "record": record}
                    for idx, metric, record in top_pool[:10]
                ]

                if selection_key_fields:
                    selected_keys = {_dedupe_key(record, selection_key_fields) for _, _, record in selected_metrics}
                    top_keys = {_dedupe_key(record, selection_key_fields) for _, _, record in top_pool}
                    missing_top = [
                        {"index": idx, "metric": metric, "record": record}
                        for idx, metric, record in top_pool
                        if _dedupe_key(record, selection_key_fields) not in selected_keys
                    ][:10]
                    selected_outside_top = [
                        {"index": idx, "metric": metric, "record": record}
                        for idx, metric, record in selected_metrics
                        if _dedupe_key(record, selection_key_fields) not in top_keys
                    ][:10]
                    check["candidate_pool_key_fields"] = selection_key_fields
                    check["missing_top_candidate_count"] = max(0, len(top_keys - selected_keys))
                    check["missing_top_candidate_examples"] = missing_top
                    check["selected_outside_top_count"] = max(0, len(selected_keys - top_keys))
                    check["selected_outside_top_examples"] = selected_outside_top
                    if missing_top or selected_outside_top:
                        audit["ready_for_done"] = False

            audit["checks"]["selection"] = check

        audit_path = _outputs_dir_path(Path(str(ns.get("cwd") or "."))) / artifact_name
        audit_path.parent.mkdir(parents=True, exist_ok=True)
        audit_path.write_text(json.dumps(audit, ensure_ascii=False, indent=2, default=str), encoding="utf-8")
        audit["audit_path"] = str(audit_path)
        copy_artifact(audit_path, kind="file", name=artifact_name, mime="application/json")
        ns["last_artifact_audit"] = audit
        emit_output(
            "artifact audit ready: "
            f"ready_for_done={audit['ready_for_done']} "
            f"records={audit['record_count']} "
            f"path={audit_path}"
        )
        return audit

    def get_final_answer() -> Any:
        return ns.get("final_answer")

    def emit_browser_live_url(live_url: str) -> None:
        _record_browser_event(ns, request_id, "browser.live_url", {"live_url": str(live_url)})

    def emit_browser_state(
        url: str | None = None,
        title: str | None = None,
        status: str | None = None,
        tabs: int | None = None,
        viewport: Any | None = None,
    ) -> None:
        payload: Dict[str, Any] = {}
        if url is not None:
            payload["url"] = str(url)
        if title is not None:
            payload["title"] = str(title)
        if status is not None:
            payload["status"] = str(status)
        if tabs is not None:
            payload["tabs"] = int(tabs)
        if viewport is not None:
            payload["viewport"] = viewport
        _record_browser_event(ns, request_id, "browser.state", payload)

    def check_cancel() -> None:
        if cancel_requested:
            raise KeyboardInterrupt("cancel requested")

    def artifact_root() -> str:
        return str(artifact_dir)

    def outputs_dir() -> str:
        path = _outputs_dir_path(Path(str(ns.get("cwd") or ".")))
        path.mkdir(parents=True, exist_ok=True)
        return str(path)

    def session_metadata() -> Dict[str, Any]:
        cwd_path = Path(str(ns.get("cwd") or "."))
        return {
            "session_id": str(ns.get("session_id")),
            "cwd": str(ns.get("cwd")),
            "artifact_root": str(artifact_dir),
            "outputs_dir": str(_outputs_dir_path(cwd_path)),
            "agent_workspace": str(_agent_workspace_path(cwd_path)),
        }

    def agent_workspace(create: bool = True) -> str:
        workspace = _agent_workspace_path(Path(str(ns.get("cwd") or ".")))
        if create:
            workspace.mkdir(parents=True, exist_ok=True)
        return str(workspace)

    def load_agent_helpers(path: Any | None = None, force: bool = True) -> Dict[str, Any]:
        return _load_agent_helpers_into_ns(ns, path=path, force=force)

    def shutdown_owned_cloud_browser() -> Dict[str, Any]:
        return _shutdown_owned_cloud_browser()

    ns.update(
        {
            "cdp": cdp,
            "emit_output": emit_output,
            "copy_artifact": copy_artifact,
            "emit_image": emit_image,
            "capture_screenshot": capture_screenshot,
            "screenshot_clip": screenshot_clip,
            "screenshot": screenshot,
            "page_info": page_info,
            "set_final_answer": set_final_answer,
            "audit_artifact": audit_artifact,
            "get_final_answer": get_final_answer,
            "emit_browser_live_url": emit_browser_live_url,
            "emit_browser_state": emit_browser_state,
            "check_cancel": check_cancel,
            "artifact_root": artifact_root,
            "outputs_dir": outputs_dir,
            "session_metadata": session_metadata,
            "agent_workspace": agent_workspace,
            "load_agent_helpers": load_agent_helpers,
            "shutdown_owned_cloud_browser": shutdown_owned_cloud_browser,
        }
    )
    _load_agent_helpers_into_ns(ns)


def _page_info_cdp_fallback(ns: Dict[str, Any], error: Exception | None) -> Dict[str, Any] | None:
    helpers = ns.get("__browser_harness_helpers__")
    if helpers is None:
        return None
    payload: Dict[str, Any] = {}
    current_target_id: str | None = None
    try:
        tab = helpers.current_tab()
        if isinstance(tab, dict):
            if tab.get("targetId"):
                current_target_id = str(tab["targetId"])
            if tab.get("url"):
                payload["url"] = str(tab["url"])
            if tab.get("title"):
                payload["title"] = str(tab["title"])
    except Exception:
        pass
    if "url" not in payload or "title" not in payload:
        try:
            targets = helpers.cdp("Target.getTargets").get("targetInfos", [])
            page_targets = [target for target in targets if target.get("type") == "page"]
            active = None
            if current_target_id:
                active = next(
                    (target for target in page_targets if str(target.get("targetId")) == current_target_id),
                    None,
                )
            if active is None:
                active = next((target for target in reversed(page_targets) if target.get("attached")), None)
            if active is None and page_targets:
                active = page_targets[-1]
            if active:
                if "url" not in payload and active.get("url"):
                    payload["url"] = str(active["url"])
                if "title" not in payload and active.get("title"):
                    payload["title"] = str(active["title"])
        except Exception:
            pass
    try:
        metrics = helpers.cdp("Page.getLayoutMetrics")
        viewport = metrics.get("cssVisualViewport") or metrics.get("cssLayoutViewport") or {}
        layout = metrics.get("cssLayoutViewport") or {}
        content = metrics.get("cssContentSize") or {}
        width = viewport.get("clientWidth") or layout.get("clientWidth")
        height = viewport.get("clientHeight") or layout.get("clientHeight")
        page_x = viewport.get("pageX") or viewport.get("offsetX") or 0
        page_y = viewport.get("pageY") or viewport.get("offsetY") or 0
        page_width = content.get("width") or width
        page_height = content.get("height") or height
        if width is not None:
            payload["w"] = round(float(width))
        if height is not None:
            payload["h"] = round(float(height))
        payload["sx"] = round(float(page_x))
        payload["sy"] = round(float(page_y))
        if page_width is not None:
            payload["pw"] = round(float(page_width))
        if page_height is not None:
            payload["ph"] = round(float(page_height))
    except Exception:
        pass
    if error is not None:
        payload["fallback"] = "cdp"
        payload["page_info_error"] = str(error)
    return payload or None


def _auto_emit_browser_state(ns: Dict[str, Any], request_id: str) -> None:
    if not ns.get("browser_harness_available"):
        return
    helpers = ns.get("__browser_harness_helpers__")
    admin = ns.get("__browser_harness_admin__")
    if helpers is None or admin is None:
        return
    try:
        if not admin.daemon_alive():
            return
        tab = helpers.current_tab()
    except Exception:
        return
    payload: Dict[str, Any] = {}
    if isinstance(tab, dict):
        if tab.get("url"):
            payload["url"] = str(tab["url"])
        if tab.get("title"):
            payload["title"] = str(tab["title"])
        if tab.get("targetId"):
            payload["target_id"] = str(tab["targetId"])
    try:
        tabs = helpers.list_tabs(include_chrome=False)
        if isinstance(tabs, list):
            payload["tabs"] = len(tabs)
    except Exception:
        pass
    try:
        metrics = helpers.cdp("Page.getLayoutMetrics")
        viewport = metrics.get("cssVisualViewport") or metrics.get("cssLayoutViewport") or {}
        width = viewport.get("clientWidth")
        height = viewport.get("clientHeight")
        if width and height:
            payload["viewport"] = {"w": round(float(width)), "h": round(float(height))}
    except Exception:
        pass
    if not payload:
        return
    record = {"type": "browser.state", "payload": payload}
    if (ns.get("browser_events") or [])[-1:] == [record]:
        return
    _record_browser_event(ns, request_id, "browser.state", payload)


def _browser_connection_snapshot(ns: Dict[str, Any]) -> Dict[str, Any] | None:
    if not ns.get("browser_harness_available"):
        return None
    helpers = ns.get("__browser_harness_helpers__")
    admin = ns.get("__browser_harness_admin__")
    if helpers is None or admin is None:
        return None
    send = getattr(helpers, "_send", None)
    if send is None:
        return None
    try:
        if not admin.daemon_alive():
            return {"status": "disconnected"}
        response = send({"meta": "connection_status"})
    except Exception as exc:
        return {"status": "disconnected", "error": str(exc)}
    if response.get("error"):
        return {"status": "disconnected", "error": str(response.get("error"))}
    page = response.get("page") or {}
    snapshot: Dict[str, Any] = {
        "status": "connected",
        "target_id": response.get("target_id"),
        "session_id": response.get("session_id"),
    }
    if page.get("url"):
        snapshot["url"] = str(page.get("url"))
    if page.get("title"):
        snapshot["title"] = str(page.get("title"))
    return snapshot


def _emit_browser_identity_events(ns: Dict[str, Any], request_id: str) -> None:
    current = _browser_connection_snapshot(ns)
    if current is None:
        return
    previous = ns.get("__browser_identity__")
    if current.get("status") != "connected":
        if isinstance(previous, dict) and previous.get("status") == "connected":
            _record_browser_event(ns, request_id, "browser.disconnected", current)
        ns["__browser_identity__"] = current
        return

    if not isinstance(previous, dict) or previous.get("status") != "connected":
        _record_browser_event(ns, request_id, "browser.connected", current)
        ns["__browser_identity__"] = current
        return

    previous_session = previous.get("session_id")
    current_session = current.get("session_id")
    previous_target = previous.get("target_id")
    current_target = current.get("target_id")
    if previous_session and current_session and previous_session != current_session:
        _record_browser_event(
            ns,
            request_id,
            "browser.reconnected",
            {
                **current,
                "previous_session_id": previous_session,
                "stale_object_ids": True,
            },
        )
    if previous_target and current_target and previous_target != current_target:
        _record_browser_event(
            ns,
            request_id,
            "browser.target_changed",
            {
                **current,
                "previous_target_id": previous_target,
                "stale_object_ids": True,
            },
        )
    ns["__browser_identity__"] = current


def _run(request: Dict[str, Any]) -> Dict[str, Any]:
    request_id = str(request.get("id") or "")
    if request.get("control") == "shutdown_owned_cloud_browser":
        return {
            "id": request_id,
            "ok": True,
            "text": "",
            "error": None,
            "data": _jsonable(_shutdown_owned_cloud_browser()),
            "outputs": [],
            "artifacts": [],
            "images": [],
            "browser_events": [],
            "browser_harness_available": False,
            "browser_harness_error": None,
        }
    session_id = str(request.get("session_id") or "default")
    cwd = Path(str(request.get("cwd") or ".")).expanduser().resolve()
    artifact_dir = Path(str(request.get("artifact_dir") or cwd / "artifacts")).expanduser().resolve()
    code = str(request.get("code") or "")
    cancel_requested = bool(request.get("cancel_requested"))
    timeout_seconds = float(request.get("timeout_seconds") or 0)
    stdout = io.StringIO()
    ns: Dict[str, Any] | None = None
    old_cwd = Path.cwd()
    old_alarm_handler: Any = None
    alarm_armed = False
    try:
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stdout):
            ns = _namespace(session_id, cwd, artifact_dir)
            _install_host_helpers(ns, request_id, cancel_requested)
        cwd.mkdir(parents=True, exist_ok=True)
        os.chdir(cwd)
        if timeout_seconds > 0 and hasattr(signal, "SIGALRM"):
            old_alarm_handler = signal.getsignal(signal.SIGALRM)
            signal.signal(signal.SIGALRM, _raise_tool_timeout)
            signal.setitimer(signal.ITIMER_REAL, timeout_seconds)
            alarm_armed = True
        assert ns is not None
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stdout):
            exec(compile(code, "<browser-use-python-worker>", "exec"), ns)
        _apply_browser_wait_between_actions()
        _auto_emit_browser_state(ns, request_id)
        _emit_browser_identity_events(ns, request_id)
        return {
            "id": request_id,
            "ok": True,
            "text": stdout.getvalue(),
            "error": None,
            "data": _jsonable(ns.get("result")),
            "outputs": _jsonable(ns.get("outputs") or []),
            "artifacts": _jsonable(ns.get("artifacts") or []),
            "images": _jsonable(ns.get("images") or []),
            "browser_events": _jsonable(ns.get("browser_events") or []),
            "browser_harness_available": bool(ns.get("browser_harness_available")),
            "browser_harness_error": ns.get("browser_harness_error"),
        }
    except BaseException as exc:
        if ns is not None:
            _emit_browser_identity_events(ns, request_id)
        return {
            "id": request_id,
            "ok": False,
            "text": stdout.getvalue(),
            "error": _annotate_error("".join(traceback.format_exception_only(type(exc), exc)).strip()),
            "data": None,
            "outputs": _jsonable((ns or {}).get("outputs") or []),
            "artifacts": _jsonable((ns or {}).get("artifacts") or []),
            "images": [],
            "browser_events": _jsonable((ns or {}).get("browser_events") or []),
            "browser_harness_available": bool((ns or {}).get("browser_harness_available")),
            "browser_harness_error": (ns or {}).get("browser_harness_error"),
        }
    finally:
        if alarm_armed:
            signal.setitimer(signal.ITIMER_REAL, 0)
            signal.signal(signal.SIGALRM, old_alarm_handler)
        os.chdir(old_cwd)


def main() -> None:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
            response = _run(request)
        except BaseException as exc:
            error = _annotate_error("".join(traceback.format_exception_only(type(exc), exc)).strip())
            response = {
                "id": "",
                "ok": False,
                "text": "",
                "error": error,
                "data": None,
                "images": [],
            }
        print(json.dumps(response, ensure_ascii=False, separators=(",", ":")), flush=True)


if __name__ == "__main__":
    main()
