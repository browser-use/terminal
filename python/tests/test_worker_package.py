import json
from pathlib import Path

from llm_browser_worker import worker


def test_worker_run_executes_in_persistent_session_namespace(tmp_path: Path) -> None:
    first = worker._run(
        {
            "id": "one",
            "session_id": "task-1",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "counter = globals().get('counter', 0) + 1\nresult = counter\nemit_output(f'counter={counter}')",
        }
    )
    second = worker._run(
        {
            "id": "two",
            "session_id": "task-1",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "counter = globals().get('counter', 0) + 1\nresult = counter",
        }
    )

    assert first["ok"] is True
    assert first["data"] == 1
    assert first["outputs"] == [{"text": "counter=1"}]
    assert second["ok"] is True
    assert second["data"] == 2


def test_worker_run_applies_browser_wait_between_actions_env(
    tmp_path: Path, monkeypatch
) -> None:
    sleeps = []
    monkeypatch.setenv("BU_BROWSER_WAIT_BETWEEN_ACTIONS_MS", "125")
    monkeypatch.setattr(worker.time, "sleep", lambda seconds: sleeps.append(seconds))

    response = worker._run(
        {
            "id": "wait-between",
            "session_id": "task-wait-between",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "result = 'ok'",
        }
    )

    assert response["ok"] is True
    assert response["data"] == "ok"
    assert sleeps == [0.125]


def test_worker_records_artifacts_and_images(tmp_path: Path) -> None:
    source = tmp_path / "source.png"
    source.write_bytes(b"png")

    response = worker._run(
        {
            "id": "image",
            "session_id": "task-2",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": f"emit_image({str(source)!r}, label='shot', mime_type='image/png')",
        }
    )

    assert response["ok"] is True
    assert response["images"][0]["label"] == "shot"
    assert response["images"][0]["mime_type"] == "image/png"
    assert Path(response["images"][0]["path"]).exists()


def test_worker_records_browser_state_details(tmp_path: Path) -> None:
    response = worker._run(
        {
            "id": "browser-state",
            "session_id": "task-3",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "emit_browser_state(url='https://example.com', title='Example', status='connected', tabs=2, viewport={'w': 1440, 'h': 900})",
        }
    )

    assert response["ok"] is True
    assert response["browser_events"] == [
        {
            "type": "browser.state",
            "payload": {
                "url": "https://example.com",
                "title": "Example",
                "status": "connected",
                "tabs": 2,
                "viewport": {"w": 1440, "h": 900},
            },
        }
    ]


def test_worker_captures_browser_harness_startup_stdout(
    tmp_path: Path, monkeypatch
) -> None:
    def fake_load_browser_harness(ns):
        print("cloud startup chatter")
        ns["browser_harness_available"] = True
        ns["browser_harness_error"] = None

    monkeypatch.setattr(worker, "_load_browser_harness", fake_load_browser_harness)

    response = worker._run(
        {
            "id": "startup-chatter",
            "session_id": "task-startup-chatter",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "result = {'ok': True}",
        }
    )

    assert response["ok"] is True
    assert "cloud startup chatter" in response["text"]
    assert response["data"] == {"ok": True}


def test_worker_capture_screenshot_attaches_image_by_default(
    tmp_path: Path, monkeypatch
) -> None:
    def fake_load_browser_harness(ns):
        def fake_capture_screenshot(path=None, full=False, max_dim=None):
            target = Path(path or "shot.png").expanduser()
            if not target.is_absolute():
                target = tmp_path / target
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(b"png")
            return str(target)

        ns["capture_screenshot"] = fake_capture_screenshot
        ns["browser_harness_available"] = True
        ns["browser_harness_error"] = None

    monkeypatch.setattr(worker, "_load_browser_harness", fake_load_browser_harness)

    response = worker._run(
        {
            "id": "attached-screenshot",
            "session_id": "task-attached-screenshot",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "path = capture_screenshot('after-click.png')\nresult = {'path': path}",
        }
    )

    assert response["ok"] is True
    assert response["data"]["path"].endswith("after-click.png")
    assert response["images"][0]["label"] == "after-click"
    assert Path(response["images"][0]["path"]).exists()


def test_worker_screenshot_shorthand_emits_labeled_image(
    tmp_path: Path, monkeypatch
) -> None:
    def fake_load_browser_harness(ns):
        def fake_capture_screenshot(path=None, full=False, max_dim=None):
            target = tmp_path / "shot.png"
            target.write_bytes(b"png")
            return str(target)

        ns["capture_screenshot"] = fake_capture_screenshot
        ns["browser_harness_available"] = True
        ns["browser_harness_error"] = None

    monkeypatch.setattr(worker, "_load_browser_harness", fake_load_browser_harness)

    response = worker._run(
        {
            "id": "screenshot-shorthand",
            "session_id": "task-screenshot-shorthand",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "image = screenshot('verified-state')\nresult = image",
        }
    )

    assert response["ok"] is True
    assert response["data"]["label"] == "verified-state"
    assert response["images"][0]["label"] == "verified-state"
    assert Path(response["images"][0]["path"]).exists()


def test_worker_screenshot_clip_uses_cdp_clip_and_attaches_image(
    tmp_path: Path, monkeypatch
) -> None:
    calls = []

    def fake_load_browser_harness(ns):
        def fake_cdp(method, **kwargs):
            calls.append((method, kwargs))
            return {"data": "cG5n"}

        ns["cdp"] = fake_cdp
        ns["browser_harness_available"] = True
        ns["browser_harness_error"] = None

    monkeypatch.setattr(worker, "_load_browser_harness", fake_load_browser_harness)

    response = worker._run(
        {
            "id": "screenshot-clip",
            "session_id": "task-screenshot-clip",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "image = screenshot_clip('table', 10, 20, 300, 120)\nresult = image",
        }
    )

    assert response["ok"] is True
    assert calls[0][0] == "Page.captureScreenshot"
    assert calls[0][1]["clip"] == {
        "x": 10.0,
        "y": 20.0,
        "width": 300.0,
        "height": 120.0,
        "scale": 1.0,
    }
    assert response["images"][0]["label"] == "table"
    assert len(response["images"]) == 1
    assert Path(response["images"][0]["path"]).exists()


def test_worker_raw_cdp_capture_screenshot_attaches_image(
    tmp_path: Path, monkeypatch
) -> None:
    def fake_load_browser_harness(ns):
        def fake_cdp(method, session_id=None, **kwargs):
            assert session_id is None
            if method == "Page.captureScreenshot":
                return {"data": "cG5n"}
            return {}

        ns["cdp"] = fake_cdp
        ns["browser_harness_available"] = True
        ns["browser_harness_error"] = None

    monkeypatch.setattr(worker, "_load_browser_harness", fake_load_browser_harness)

    response = worker._run(
        {
            "id": "raw-cdp-screenshot",
            "session_id": "task-raw-cdp-screenshot",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "result = cdp('Page.captureScreenshot', format='png')",
        }
    )

    assert response["ok"] is True
    assert response["data"] == {"data": "cG5n"}
    assert len(response["images"]) == 1
    assert response["images"][0]["label"] == "cdp_screenshot_1"
    assert Path(response["images"][0]["path"]).exists()


def test_worker_cdp_applies_browser_user_agent_env(monkeypatch) -> None:
    calls = []
    monkeypatch.setenv("BU_BROWSER_USER_AGENT", " BrowserUseRuntime/3.0 ")
    monkeypatch.setenv("LLM_BROWSER_BROWSER_MODE", "remote-cdp")

    class Helpers:
        __all__ = ["cdp"]

        def cdp(self, method, session_id=None, **params):
            calls.append((method, session_id, params))
            return {"method": method}

    class Admin:
        def __init__(self) -> None:
            self.ensure_calls = 0

        def ensure_daemon(self):
            self.ensure_calls += 1

    helpers = Helpers()
    admin = Admin()
    worker._patch_browser_harness_cdp(helpers, admin)

    result = helpers.cdp("Runtime.evaluate", session_id="target-1", expression="navigator.userAgent")

    assert result == {"method": "Runtime.evaluate"}
    assert admin.ensure_calls == 1
    assert calls == [
        ("Network.setUserAgentOverride", "target-1", {"userAgent": "BrowserUseRuntime/3.0"}),
        ("Runtime.evaluate", "target-1", {"expression": "navigator.userAgent"}),
    ]

    calls.clear()
    result = helpers.cdp("Network.setUserAgentOverride", userAgent="Manual/1.0")

    assert result == {"method": "Network.setUserAgentOverride"}
    assert admin.ensure_calls == 2
    assert calls == [("Network.setUserAgentOverride", None, {"userAgent": "Manual/1.0"})]


def test_worker_cdp_grants_browser_permissions_env(monkeypatch) -> None:
    calls = []
    monkeypatch.setenv(
        "BU_BROWSER_PERMISSIONS",
        '["clipboardReadWrite","notifications","clipboardReadWrite",3,""]',
    )
    monkeypatch.setenv("LLM_BROWSER_BROWSER_MODE", "remote-cdp")

    class Helpers:
        __all__ = ["cdp"]

        def cdp(self, method, session_id=None, **params):
            calls.append((method, session_id, params))
            return {"method": method}

    class Admin:
        def __init__(self) -> None:
            self.ensure_calls = 0

        def ensure_daemon(self):
            self.ensure_calls += 1

    helpers = Helpers()
    admin = Admin()
    worker._patch_browser_harness_cdp(helpers, admin)

    result = helpers.cdp("Page.navigate", session_id="target-1", url="https://example.com")

    assert result == {"method": "Page.navigate"}
    assert admin.ensure_calls == 1
    assert calls == [
        (
            "Browser.grantPermissions",
            None,
            {"permissions": ["clipboardReadWrite", "notifications"]},
        ),
        ("Page.navigate", "target-1", {"url": "https://example.com"}),
    ]

    calls.clear()
    result = helpers.cdp("Browser.grantPermissions", permissions=["geolocation"])

    assert result == {"method": "Browser.grantPermissions"}
    assert admin.ensure_calls == 2
    assert calls == [("Browser.grantPermissions", None, {"permissions": ["geolocation"]})]


def test_worker_cdp_applies_browser_download_behavior_env(tmp_path: Path, monkeypatch) -> None:
    calls = []
    downloads_path = tmp_path / "downloads"
    monkeypatch.setenv("BU_BROWSER_ACCEPT_DOWNLOADS", "true")
    monkeypatch.setenv("BU_BROWSER_DOWNLOADS_PATH", str(downloads_path))
    monkeypatch.setenv("LLM_BROWSER_BROWSER_MODE", "remote-cdp")

    class Helpers:
        __all__ = ["cdp"]

        def cdp(self, method, session_id=None, **params):
            calls.append((method, session_id, params))
            return {"method": method}

    class Admin:
        def __init__(self) -> None:
            self.ensure_calls = 0

        def ensure_daemon(self):
            self.ensure_calls += 1

    helpers = Helpers()
    admin = Admin()
    worker._patch_browser_harness_cdp(helpers, admin)

    result = helpers.cdp("Page.navigate", session_id="target-1", url="https://example.com")

    assert result == {"method": "Page.navigate"}
    assert admin.ensure_calls == 1
    assert calls == [
        (
            "Browser.setDownloadBehavior",
            None,
            {
                "behavior": "allow",
                "downloadPath": str(downloads_path.resolve()),
                "eventsEnabled": True,
            },
        ),
        ("Page.navigate", "target-1", {"url": "https://example.com"}),
    ]
    assert downloads_path.exists()

    calls.clear()
    monkeypatch.setenv("BU_BROWSER_ACCEPT_DOWNLOADS", "false")
    result = helpers.cdp("Runtime.evaluate", expression="1")

    assert result == {"method": "Runtime.evaluate"}
    assert admin.ensure_calls == 2
    assert calls == [
        ("Browser.setDownloadBehavior", None, {"behavior": "deny"}),
        ("Runtime.evaluate", None, {"expression": "1"}),
    ]

    calls.clear()
    result = helpers.cdp("Browser.setDownloadBehavior", behavior="allowAndName")

    assert result == {"method": "Browser.setDownloadBehavior"}
    assert admin.ensure_calls == 3
    assert calls == [("Browser.setDownloadBehavior", None, {"behavior": "allowAndName"})]


def test_worker_cdp_applies_browser_storage_state_env(monkeypatch) -> None:
    calls = []
    cookie = {"name": "sid", "value": "secret", "domain": ".example.com", "path": "/"}
    monkeypatch.setenv(
        "BU_BROWSER_STORAGE_STATE",
        json.dumps(
            {
                "cookies": [cookie, {"name": "bad"}],
                "origins": [
                    {
                        "origin": "https://example.com",
                        "localStorage": [{"name": "theme", "value": "dark"}, {"name": 3}],
                        "sessionStorage": [{"name": "step", "value": "one"}],
                    }
                ],
            }
        ),
    )
    monkeypatch.setenv("LLM_BROWSER_BROWSER_MODE", "remote-cdp")

    class Helpers:
        __all__ = ["cdp"]

        def cdp(self, method, session_id=None, **params):
            calls.append((method, session_id, params))
            return {"method": method}

    class Admin:
        def __init__(self) -> None:
            self.ensure_calls = 0

        def ensure_daemon(self):
            self.ensure_calls += 1

    helpers = Helpers()
    admin = Admin()
    worker._patch_browser_harness_cdp(helpers, admin)

    result = helpers.cdp("Page.navigate", session_id="target-1", url="https://example.com")

    assert result == {"method": "Page.navigate"}
    assert admin.ensure_calls == 1
    assert calls[0] == ("Storage.setCookies", "target-1", {"cookies": [cookie]})
    assert calls[1][0] == "Page.addScriptToEvaluateOnNewDocument"
    assert calls[1][1] == "target-1"
    assert calls[1][2]["runImmediately"] is True
    script = calls[1][2]["source"]
    assert 'window.location.origin === "https://example.com"' in script
    assert 'window.localStorage.setItem("theme", "dark");' in script
    assert 'window.sessionStorage.setItem("step", "one");' in script
    assert calls[2] == ("Page.navigate", "target-1", {"url": "https://example.com"})

    calls.clear()
    result = helpers.cdp("Runtime.evaluate", session_id="target-1", expression="1")

    assert result == {"method": "Runtime.evaluate"}
    assert admin.ensure_calls == 2
    assert calls == [("Runtime.evaluate", "target-1", {"expression": "1"})]

    calls.clear()
    result = helpers.cdp("Storage.setCookies", session_id="target-1", cookies=[])

    assert result == {"method": "Storage.setCookies"}
    assert admin.ensure_calls == 3
    assert calls == [("Storage.setCookies", "target-1", {"cookies": []})]


def test_worker_cdp_enforces_browser_domain_constraints_env(monkeypatch) -> None:
    calls = []
    monkeypatch.setenv("LLM_BROWSER_BROWSER_MODE", "remote-cdp")
    monkeypatch.setenv("BU_BROWSER_ALLOWED_DOMAINS", '["example.com","*.browser-use.com"]')

    class Helpers:
        __all__ = ["cdp"]

        def cdp(self, method, session_id=None, **params):
            calls.append((method, session_id, params))
            return {"method": method}

    class Admin:
        def __init__(self) -> None:
            self.ensure_calls = 0

        def ensure_daemon(self):
            self.ensure_calls += 1

    def expect_blocked(url: str, method: str = "Page.navigate") -> None:
        try:
            helpers.cdp(method, session_id="target-1", url=url)
        except RuntimeError as exc:
            assert "BrowserProfile domain constraints blocked navigation" in str(exc)
            assert url in str(exc)
        else:
            raise AssertionError(f"navigation should be blocked: {url}")

    helpers = Helpers()
    admin = Admin()
    worker._patch_browser_harness_cdp(helpers, admin)

    result = helpers.cdp("Page.navigate", session_id="target-1", url="https://www.example.com/path")

    assert result == {"method": "Page.navigate"}
    assert calls == [("Page.navigate", "target-1", {"url": "https://www.example.com/path"})]

    expect_blocked("https://iana.org/")
    expect_blocked("https://example.com.evil.org/")

    assert len(calls) == 1

    monkeypatch.setenv("BU_BROWSER_PROHIBITED_DOMAINS", '["www.example.com"]')
    expect_blocked("https://www.example.com/path")

    assert len(calls) == 1

    monkeypatch.delenv("BU_BROWSER_ALLOWED_DOMAINS", raising=False)
    monkeypatch.setenv("BU_BROWSER_PROHIBITED_DOMAINS", '["*.tracking.example"]')

    expect_blocked("https://ads.tracking.example/")

    assert len(calls) == 1

    helpers.cdp("Target.createTarget", url="https://safe.example/")
    assert calls[-1] == ("Target.createTarget", None, {"url": "https://safe.example/"})

    expect_blocked("https://ads.tracking.example/", method="Target.createTarget")

    monkeypatch.delenv("BU_BROWSER_PROHIBITED_DOMAINS", raising=False)
    monkeypatch.setenv("BU_BROWSER_BLOCK_IP_ADDRESSES", "true")

    expect_blocked("http://127.0.0.1/")

    assert len(calls) == 2
    assert admin.ensure_calls == 8


def test_worker_page_info_fallback_reads_target_url_and_title(
    tmp_path: Path, monkeypatch
) -> None:
    class FakeHelpers:
        def current_tab(self):
            return {"targetId": "target-2"}

        def cdp(self, method, **kwargs):
            if method == "Target.getTargets":
                return {
                    "targetInfos": [
                        {
                            "targetId": "target-1",
                            "type": "page",
                            "attached": True,
                            "url": "https://old.example/",
                            "title": "Old",
                        },
                        {
                            "targetId": "target-2",
                            "type": "page",
                            "attached": True,
                            "url": "https://example.com/",
                            "title": "Example",
                        },
                    ]
                }
            if method == "Page.getLayoutMetrics":
                return {"cssVisualViewport": {"clientWidth": 800, "clientHeight": 600}}
            raise AssertionError(method)

    def fake_load_browser_harness(ns):
        ns["page_info"] = lambda: (_ for _ in ()).throw(RuntimeError("page JS wedged"))
        ns["__browser_harness_helpers__"] = FakeHelpers()
        ns["browser_harness_available"] = True
        ns["browser_harness_error"] = None

    monkeypatch.setattr(worker, "_load_browser_harness", fake_load_browser_harness)

    response = worker._run(
        {
            "id": "page-info-fallback",
            "session_id": "task-page-info-fallback",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "result = page_info()",
        }
    )

    assert response["ok"] is True
    assert response["data"]["url"] == "https://example.com/"
    assert response["data"]["title"] == "Example"
    assert response["data"]["w"] == 800
    assert response["data"]["h"] == 600
    assert response["data"]["fallback"] == "cdp"


def test_worker_autoloads_agent_workspace_helpers(tmp_path: Path, monkeypatch) -> None:
    workspace = tmp_path / ".browser-use-terminal" / "agent-workspace"
    monkeypatch.setenv("BH_AGENT_WORKSPACE", str(workspace))
    workspace.mkdir(parents=True)
    (workspace / "agent_helpers.py").write_text(
        "def helper_value():\n    return 42\n",
        encoding="utf-8",
    )

    response = worker._run(
        {
            "id": "agent-helpers",
            "session_id": "task-agent-helpers",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "result = {'workspace': agent_workspace(create=False), 'value': helper_value()}",
        }
    )

    assert response["ok"] is True
    assert response["data"]["workspace"] == str(workspace)
    assert response["data"]["value"] == 42


def test_worker_error_hints_are_appended(tmp_path: Path) -> None:
    cases = [
        (
            "raise RuntimeError(\"':contains' is not a valid CSS selector\")",
            "':contains' is jQuery, not CSS.",
        ),
        (
            "raise RuntimeError(\"Identifier 'buttons' has already been declared\")",
            "execution contexts persist",
        ),
        (
            "raise RuntimeError('Blocked a frame with origin https://a from accessing a cross-origin frame')",
            "Cross-origin iframe DOM access",
        ),
        (
            "raise RuntimeError('-32602 No target with given id found')",
            "target closed or was replaced",
        ),
        (
            "raise RuntimeError(\"Runtime.getExecutionContexts wasn't found\")",
            "Runtime.getExecutionContexts is not a CDP method",
        ),
    ]

    for idx, (code, expected_hint) in enumerate(cases):
        response = worker._run(
            {
                "id": f"hint-{idx}",
                "session_id": f"task-hint-{idx}",
                "cwd": str(tmp_path),
                "artifact_dir": str(tmp_path / "artifacts"),
                "code": code,
            }
        )
        assert response["ok"] is False
        assert "Hint:" in response["error"]
        assert expected_hint in response["error"]


def test_worker_set_final_answer_persists_metadata_and_compact_result(tmp_path: Path) -> None:
    response = worker._run(
        {
            "id": "final-answer",
            "session_id": "task-final-answer",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "summary = set_final_answer({'stores': [{'name': 'A', 'address': 'B'}]}, artifact_name='stores.json')\nresult = summary",
        }
    )

    assert response["ok"] is True
    assert response["data"]["count"] == 1
    assert response["outputs"][0]["text"].startswith("final answer ready:")
    assert Path(response["data"]["artifact"]["path"]).exists()
    metadata = tmp_path / "artifacts" / ".final_answer.json"
    assert metadata.exists()
    assert '"stores"' in metadata.read_text()


def test_worker_audit_artifact_reports_general_quality_checks(tmp_path: Path) -> None:
    response = worker._run(
        {
            "id": "artifact-audit",
            "session_id": "task-artifact-audit",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": """
rows = [
    {'name': 'A', 'category': 'one', 'score': 10},
    {'name': '', 'category': 'one', 'score': 7},
    {'name': 'A', 'category': 'two', 'score': 3},
]
audit = audit_artifact(
    records=rows,
    required_fields=['name', 'category'],
    dedupe_fields=['name'],
    bucket_field='category',
    bucket_targets={'one': 3, 'two': 1},
)
result = audit
""",
        }
    )

    assert response["ok"] is True
    audit = response["data"]
    assert audit["ready_for_done"] is False
    assert audit["generated_by"] == "audit_artifact"
    assert audit["record_count"] == 3
    assert audit["checks"]["missing_fields"]["name"]["count"] == 1
    assert audit["checks"]["dedupe"]["duplicate_count"] == 1
    assert audit["checks"]["buckets"]["unmet_targets"] == {
        "one": {"count": 2, "target": 3}
    }
    assert Path(audit["audit_path"]).exists()
    assert response["artifacts"][0]["source_path"] == audit["audit_path"]


def test_worker_audit_artifact_zero_records_requires_explicit_empty_proof(
    tmp_path: Path,
) -> None:
    blocked = worker._run(
        {
            "id": "artifact-zero-record-audit",
            "session_id": "task-artifact-zero-record-audit",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "audit = audit_artifact(records=[], required_fields=['name'])\nresult = audit",
        }
    )
    assert blocked["data"]["ready_for_done"] is False
    assert blocked["data"]["checks"]["record_count"]["violation"] == "zero_records"

    allowed = worker._run(
        {
            "id": "artifact-zero-record-audit-allowed",
            "session_id": "task-artifact-zero-record-audit-allowed",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts2"),
            "code": "audit = audit_artifact(records=[], required_fields=['name'], allow_empty=True)\nresult = audit",
        }
    )
    assert allowed["data"]["ready_for_done"] is True


def test_worker_set_final_answer_embeds_explicit_audit(tmp_path: Path) -> None:
    response = worker._run(
        {
            "id": "final-answer-audit",
            "session_id": "task-final-answer-audit",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "rows=[{'name':''}]\naudit=audit_artifact(records=rows, required_fields=['name'])\nsummary=set_final_answer({'rows': rows}, artifact_name='rows.json', audit=audit)\nresult=summary",
        }
    )

    assert response["ok"] is True
    assert response["data"]["ready_for_done"] is False
    assert response["data"]["audit"]["checks"]["missing_fields"]["name"]["count"] == 1
    assert "audit_ready_for_done=False" in response["outputs"][-1]["text"]


def test_worker_audit_artifact_reports_selection_metric_gaps(tmp_path: Path) -> None:
    response = worker._run(
        {
            "id": "artifact-selection-audit",
            "session_id": "task-artifact-selection-audit",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": """
selected = [{'id': 'b', 'score': 7}, {'id': 'a', 'score': 10}]
pool = [{'id': 'a', 'score': 10}, {'id': 'b', 'score': 7}, {'id': 'c', 'score': 11}]
audit = audit_artifact(
    records=selected,
    selection_metric_field='score',
    selection_order='desc',
    selection_limit=2,
    selection_pool_records=pool,
    selection_key_fields=['id'],
)
result = audit
""",
        }
    )

    audit = response["data"]
    assert audit["ready_for_done"] is False
    selection = audit["checks"]["selection"]
    assert selection["order_violation_count"] == 1
    assert selection["missing_top_candidate_count"] == 1
    assert selection["selected_outside_top_count"] == 1


def test_managed_browser_does_not_use_system_chromium_without_opt_in(
    tmp_path: Path, monkeypatch
) -> None:
    system_chromium = tmp_path / "chromium"
    system_chromium.write_text("#!/bin/sh\n")
    monkeypatch.delenv("CHROME_PATH", raising=False)
    monkeypatch.delenv("LLM_BROWSER_ALLOW_SYSTEM_CHROMIUM", raising=False)
    monkeypatch.delenv("LLM_BROWSER_ALLOW_GOOGLE_CHROME", raising=False)
    monkeypatch.setattr(worker, "_playwright_chromium_candidates", lambda: [])
    monkeypatch.setattr(worker.shutil, "which", lambda name: str(system_chromium))

    try:
        worker._pick_chromium_path()
    except RuntimeError as exc:
        assert "Playwright Chromium not found" in str(exc)
    else:
        raise AssertionError("system Chromium should require explicit opt-in")

    monkeypatch.setenv("LLM_BROWSER_ALLOW_SYSTEM_CHROMIUM", "1")
    assert worker._pick_chromium_path()


def test_visible_managed_browser_prefers_google_chrome(monkeypatch) -> None:
    monkeypatch.delenv("CHROME_PATH", raising=False)

    class FakePath:
        def __init__(self, value: str) -> None:
            self.value = value

        def exists(self) -> bool:
            return self.value == "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"

        def __str__(self) -> str:
            return self.value

    monkeypatch.setattr(worker, "Path", FakePath)

    assert (
        worker._pick_managed_chrome_path(visible=True)
        == "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
    )


def test_managed_browser_profile_env_controls_worker_launch(
    tmp_path: Path, monkeypatch
) -> None:
    monkeypatch.delenv("BU_CDP_URL", raising=False)
    monkeypatch.delenv("BU_CDP_WS", raising=False)
    monkeypatch.delenv("BU_BROWSER_ID", raising=False)
    monkeypatch.setenv("LLM_BROWSER_BROWSER_MODE", "managed_headed")
    monkeypatch.setenv(
        "BU_MANAGED_BROWSER_ARGS",
        '["--proxy-server=http://proxy.example:8080","--user-agent=BrowserUseTest/1.0",3,""]',
    )

    assert worker._should_start_managed_chrome() is True
    assert worker._managed_chrome_is_visible() is True

    headed = worker._managed_chrome_args("/chrome", 9335, tmp_path / "profile", True)
    assert "--new-window" in headed
    assert "--proxy-server=http://proxy.example:8080" in headed
    assert "--user-agent=BrowserUseTest/1.0" in headed
    assert headed.index("--proxy-server=http://proxy.example:8080") < headed.index("about:blank")
    assert "3" not in headed
    assert "" not in headed

    monkeypatch.setenv("LLM_BROWSER_BROWSER_MODE", "managed-headless")

    assert worker._should_start_managed_chrome() is True
    assert worker._managed_chrome_is_visible() is False

    headless = worker._managed_chrome_args("/chrome", 9336, tmp_path / "profile", False)
    assert "--headless=new" in headless
    assert "--new-window" not in headless
    assert "--proxy-server=http://proxy.example:8080" in headless


def test_managed_browser_profile_env_uses_configured_user_data_dir(
    tmp_path: Path, monkeypatch
) -> None:
    configured_profile = tmp_path / "configured-profile"
    monkeypatch.setenv("BU_MANAGED_BROWSER_PROFILE", str(configured_profile))

    profile, is_temporary = worker._managed_chrome_profile_dir()

    assert profile == configured_profile
    assert is_temporary is False
    assert configured_profile.exists()

    args = worker._managed_chrome_args("/chrome", 9337, profile, False)
    assert f"--user-data-dir={configured_profile}" in args

    monkeypatch.setattr(worker, "_managed_chrome", None)
    monkeypatch.setattr(worker, "_managed_chrome_profile", configured_profile)
    monkeypatch.setattr(worker, "_managed_chrome_profile_is_temporary", False)

    worker._cleanup_managed_chrome()

    assert configured_profile.exists()
    assert worker._managed_chrome_profile is None
    assert worker._managed_chrome_profile_is_temporary is False


def test_managed_browser_viewport_env_controls_worker_launch(
    tmp_path: Path, monkeypatch
) -> None:
    monkeypatch.setenv(
        "BU_BROWSER_VIEWPORT",
        '{"width":1024,"height":768,"deviceScaleFactor":2,"screenWidth":1440,"screenHeight":900}',
    )
    monkeypatch.setenv("BU_BROWSER_NO_VIEWPORT", "false")

    headless = worker._managed_chrome_args("/chrome", 9338, tmp_path / "profile", False)

    assert "--headless=new" in headless
    assert "--window-size=1024,768" in headless
    assert "--force-device-scale-factor=2" in headless

    headed = worker._managed_chrome_args("/chrome", 9339, tmp_path / "profile", True)

    assert "--new-window" in headed
    assert "--window-size=1024,768" in headed
    assert "--window-size=1512,900" not in headed

    monkeypatch.setenv("BU_BROWSER_NO_VIEWPORT", "true")

    no_viewport = worker._managed_chrome_args("/chrome", 9340, tmp_path / "profile", True)

    assert "--window-size=1024,768" not in no_viewport
    assert "--force-device-scale-factor=2" not in no_viewport
    assert "--window-size=1512,900" in no_viewport


def test_managed_chrome_args_visible_vs_headless(tmp_path: Path) -> None:
    visible = worker._managed_chrome_args("/chrome", 9333, tmp_path / "profile", True)
    headless = worker._managed_chrome_args("/chrome", 9334, tmp_path / "profile", False)

    assert "--new-window" in visible
    assert "--window-size=1512,900" in visible
    assert "--headless=new" not in visible
    assert "--headless=new" in headless
    assert "--new-window" not in headless
