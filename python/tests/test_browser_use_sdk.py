from __future__ import annotations

import asyncio
import json
from pathlib import Path
from typing import Any, Dict, List, Optional

import pytest

from browser_use import (
    Agent,
    AgentHistoryList,
    Browser,
    BrowserAlreadyInUseError,
    ChatBrowserUse,
    Client,
)
from browser_use.runtime import JsonRpcError, RuntimeClient


class FakeRuntime:
    def __init__(
        self,
        *,
        run_delay: float = 0.0,
        run_result: Optional[Dict[str, Any]] = None,
    ):
        self.calls: List[tuple[str, Dict[str, Any]]] = []
        self.run_delay = run_delay
        self.run_result = run_result or {"history": {"output": "done", "success": True}}
        self.next_browser = 1
        self.next_agent = 1
        self.queues: Dict[str, asyncio.Queue[Dict[str, Any]]] = {}
        self.projected_queues: Dict[str, asyncio.Queue[Dict[str, Any]]] = {}
        self.settings: Dict[str, Any] = {
            "backend": "managed-headless",
            "mode": "managed-headless",
            "browser": "Headless Chromium",
            "local_browser": None,
            "profile_id": None,
            "profile_label": None,
        }

    async def call(self, method: str, params: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
        payload = dict(params or {})
        self.calls.append((method, payload))
        if method == "browser.settings":
            return dict(self.settings)
        if method == "browser.set_backend":
            backend = str(payload.get("backend") or payload.get("mode"))
            self.settings.update(
                {
                    "backend": backend,
                    "mode": backend,
                    "browser": backend,
                    "local_browser": payload.get("local_browser"),
                }
            )
            return dict(self.settings)
        if method == "browser.set_profile":
            self.settings.update(
                {
                    "backend": payload.get("backend") or "local",
                    "mode": payload.get("backend") or "local",
                    "profile_id": payload.get("profile_id"),
                    "profile_label": payload.get("profile_label"),
                    "local_browser": payload.get("local_browser"),
                }
            )
            return dict(self.settings)
        if method == "browser.create":
            browser_id = f"browser-{self.next_browser}"
            self.next_browser += 1
            return {"browser_id": browser_id}
        if method == "agent.create":
            agent_id = f"agent-{self.next_agent}"
            self.next_agent += 1
            return {"agent_id": agent_id, "session_id": f"session-{self.next_agent}"}
        if method == "agent.snapshot":
            return {
                "agent_id": payload.get("agent_id", "agent-1"),
                "session_id": "session-2",
                "status": "created",
                "live": {"pending_mailbox_count": 0},
            }
        if method in {"agent.run", "agent.run_task"}:
            if self.run_delay:
                queue = self.projected_event_queue("session-2")
                queue.put_nowait(
                    {
                        "kind": "thread_status_changed",
                        "payload": {"status": "running"},
                    }
                )
                await asyncio.sleep(self.run_delay)
            return self.run_result
        return {}

    async def close(self) -> None:
        self.calls.append(("runtime.close", {}))

    def event_queue(self, run_id: str) -> asyncio.Queue[Dict[str, Any]]:
        return self.queues.setdefault(run_id, asyncio.Queue())

    def projected_event_queue(self, run_id: str) -> asyncio.Queue[Dict[str, Any]]:
        return self.projected_queues.setdefault(run_id, asyncio.Queue())


def test_llm_protocol_preserves_model_options() -> None:
    llm = ChatBrowserUse(model="bu-2-0", base_url="https://example.invalid", temperature=0)

    assert llm.to_protocol() == {
        "provider": "browser-use",
        "model": "bu-2-0",
        "base_url": "https://example.invalid",
        "timeout": 120.0,
        "options": {"temperature": 0},
    }


def test_browser_start_creates_rust_browser() -> None:
    asyncio.run(_test_browser_start_creates_rust_browser())


async def _test_browser_start_creates_rust_browser() -> None:
    runtime = FakeRuntime()
    browser = Browser(headless=True, keep_alive=True, _runtime=runtime)  # type: ignore[arg-type]

    await browser.start()

    assert browser.browser_id == "browser-1"
    assert runtime.calls == [
        ("browser.create", {"headless": True, "keep_alive": True}),
    ]


def test_browser_start_preserves_extended_options() -> None:
    asyncio.run(_test_browser_start_preserves_extended_options())


async def _test_browser_start_preserves_extended_options() -> None:
    runtime = FakeRuntime()
    browser = Browser(
        cdp_url="wss://browser.example.test/cdp",
        cdp_headers={"Authorization": "Bearer test"},
        user_agent="BrowserUseTest/1.0",
        no_viewport=True,
        accept_downloads=True,
        _runtime=runtime,  # type: ignore[arg-type]
    )

    await browser.start()

    assert runtime.calls[0] == (
        "browser.create",
        {
            "keep_alive": False,
            "cdp_url": "wss://browser.example.test/cdp",
            "cdp_headers": {"Authorization": "Bearer test"},
            "user_agent": "BrowserUseTest/1.0",
            "no_viewport": True,
            "accept_downloads": True,
        },
    )


def test_browser_stop_clears_runtime_browser_id() -> None:
    asyncio.run(_test_browser_stop_clears_runtime_browser_id())


async def _test_browser_stop_clears_runtime_browser_id() -> None:
    runtime = FakeRuntime()
    browser = Browser(_runtime=runtime)  # type: ignore[arg-type]

    await browser.start()
    await browser.stop()

    assert browser.browser_id is None
    assert [call[0] for call in runtime.calls] == ["browser.create", "browser.stop"]


def test_agent_run_uses_browser_and_returns_history() -> None:
    asyncio.run(_test_agent_run_uses_browser_and_returns_history())


async def _test_agent_run_uses_browser_and_returns_history() -> None:
    runtime = FakeRuntime(run_result={"history": {"output": "Paris", "success": True}})
    browser = Browser(_runtime=runtime)  # type: ignore[arg-type]
    agent = Agent("capital?", llm=ChatBrowserUse(model="bu-2-0"), browser=browser)

    history = await agent.run(max_steps=3)

    assert history.final_result() == "Paris"
    assert history.output == "Paris"
    assert history.is_done()
    assert history.is_successful()
    assert [call[0] for call in runtime.calls] == [
        "browser.create",
        "agent.create",
        "agent.run",
    ]
    assert runtime.calls[-1][1]["browser_id"] == "browser-1"
    assert runtime.calls[-1][1]["max_steps"] == 3


def test_client_browser_controls_call_rust_settings_methods() -> None:
    asyncio.run(_test_client_browser_controls_call_rust_settings_methods())


async def _test_client_browser_controls_call_rust_settings_methods() -> None:
    runtime = FakeRuntime()
    client = Client(_runtime=runtime)  # type: ignore[arg-type]

    settings = await client.browser.settings()
    backend = await client.browser.set_backend("cloud")
    profile = await client.browser.set_profile(
        "google-chrome:Default",
        profile_label="Default",
        local_browser="Google Chrome",
    )

    assert settings["backend"] == "managed-headless"
    assert backend["backend"] == "cloud"
    assert profile["profile_id"] == "google-chrome:Default"
    assert [call[0] for call in runtime.calls] == [
        "browser.settings",
        "browser.set_backend",
        "browser.set_profile",
    ]


def test_client_agent_run_task_uses_one_shot_runtime_method() -> None:
    asyncio.run(_test_client_agent_run_task_uses_one_shot_runtime_method())


async def _test_client_agent_run_task_uses_one_shot_runtime_method() -> None:
    runtime = FakeRuntime(run_result={"history": {"output": "done", "success": True}})
    client = Client(_runtime=runtime)  # type: ignore[arg-type]

    history = await client.agent.run(
        "summarize",
        llm=ChatBrowserUse(model="fake"),
        max_steps=7,
        browser=Browser(cdp_url="wss://browser.example.test/cdp", _runtime=runtime),  # type: ignore[arg-type]
    )

    assert history.final_result() == "done"
    method, payload = runtime.calls[-1]
    assert method == "agent.run_task"
    assert payload["task"] == "summarize"
    assert payload["max_steps"] == 7
    assert payload["llm"]["model"] == "fake"
    assert payload["browser"]["cdp_url"] == "wss://browser.example.test/cdp"


def test_agent_stream_yields_runtime_events_and_final_history() -> None:
    asyncio.run(_test_agent_stream_yields_runtime_events_and_final_history())


async def _test_agent_stream_yields_runtime_events_and_final_history() -> None:
    runtime = FakeRuntime(
        run_delay=0.01,
        run_result={
            "history": {"output": "streamed", "success": True},
            "final_projected_event": {
                "kind": "turn_completed",
                "payload": {"success": True, "result": "streamed"},
                "snapshot": {
                    "agents": [
                        {
                            "agent_id": "agent-1",
                            "session_id": "session-2",
                            "status": "completed",
                        }
                    ]
                },
            },
        },
    )
    browser = Browser(_runtime=runtime)  # type: ignore[arg-type]
    agent = Agent("stream?", llm=ChatBrowserUse(model="bu-2-0"), browser=browser)

    events = [event async for event in agent.stream(max_steps=4)]

    assert events[0]["type"] == "agent.snapshot"
    assert events[1]["kind"] == "thread_status_changed"
    assert events[-2]["kind"] == "turn_completed"
    assert events[-1]["type"] == "agent.completed"
    assert events[-1]["output"] == "streamed"
    assert events[-1]["history"].final_result() == "streamed"
    assert runtime.calls[-1][0] == "agent.run"
    assert runtime.calls[-1][1]["max_steps"] == 4


def test_runtime_client_routes_agent_events_by_run_and_session_id() -> None:
    runtime = RuntimeClient(command=["unused"])

    runtime._handle_message(
        {
            "jsonrpc": "2.0",
            "method": "agent.event",
            "params": {
                "run_id": "run-1",
                "session_id": "session-1",
                "event": {"kind": "agent_started"},
            },
        }
    )

    assert runtime.event_queue("run-1").get_nowait() == {"kind": "agent_started"}
    assert runtime.event_queue("session-1").get_nowait() == {"kind": "agent_started"}


def test_runtime_client_routes_projected_agent_events_by_run_and_session_id() -> None:
    runtime = RuntimeClient(command=["unused"])

    runtime._handle_message(
        {
            "jsonrpc": "2.0",
            "method": "agent.projected_event",
            "params": {
                "run_id": "run-1",
                "session_id": "session-1",
                "event": {"kind": "thread_status_changed"},
            },
        }
    )

    assert runtime.projected_event_queue("run-1").get_nowait() == {
        "kind": "thread_status_changed"
    }
    assert runtime.projected_event_queue("session-1").get_nowait() == {
        "kind": "thread_status_changed"
    }


def test_runtime_client_cancelled_call_removes_pending_request() -> None:
    asyncio.run(_test_runtime_client_cancelled_call_removes_pending_request())


async def _test_runtime_client_cancelled_call_removes_pending_request() -> None:
    class FakeStdin:
        def __init__(self) -> None:
            self.lines: List[bytes] = []

        def write(self, data: bytes) -> None:
            self.lines.append(data)

        async def drain(self) -> None:
            return None

    class FakeProcess:
        def __init__(self) -> None:
            self.stdin = FakeStdin()
            self.returncode = None

    class NoStartRuntime(RuntimeClient):
        async def start(self) -> None:
            self._process = FakeProcess()  # type: ignore[assignment]

    runtime = NoStartRuntime(command=["unused"])
    task = asyncio.create_task(runtime.call("runtime.never_returns"))
    await asyncio.sleep(0)

    assert list(runtime._pending) == [1]
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    assert runtime._pending == {}


def test_two_running_agents_cannot_share_one_browser() -> None:
    asyncio.run(_test_two_running_agents_cannot_share_one_browser())


async def _test_two_running_agents_cannot_share_one_browser() -> None:
    runtime = FakeRuntime(run_delay=0.05)
    browser = Browser(_runtime=runtime)  # type: ignore[arg-type]
    first = Agent("first", browser=browser)
    second = Agent("second", browser=browser)

    task = asyncio.create_task(first.run())
    await asyncio.sleep(0)

    with pytest.raises(BrowserAlreadyInUseError):
        await second.run()

    await task


def test_two_agents_with_different_browsers_can_run_concurrently() -> None:
    asyncio.run(_test_two_agents_with_different_browsers_can_run_concurrently())


async def _test_two_agents_with_different_browsers_can_run_concurrently() -> None:
    runtime = FakeRuntime(run_delay=0.02)
    first = Agent("first", browser=Browser(_runtime=runtime))  # type: ignore[arg-type]
    second = Agent("second", browser=Browser(_runtime=runtime))  # type: ignore[arg-type]

    first_history, second_history = await asyncio.gather(first.run(), second.run())

    assert first_history.final_result() == "done"
    assert second_history.final_result() == "done"
    run_calls = [payload for method, payload in runtime.calls if method == "agent.run"]
    assert len(run_calls) == 2
    assert {call["browser_id"] for call in run_calls} == {"browser-1", "browser-2"}


def test_agent_run_cancellation_requests_runtime_stop() -> None:
    asyncio.run(_test_agent_run_cancellation_requests_runtime_stop())


async def _test_agent_run_cancellation_requests_runtime_stop() -> None:
    runtime = FakeRuntime(run_delay=1.0)
    agent = Agent("cancel me", browser=Browser(_runtime=runtime))  # type: ignore[arg-type]

    task = asyncio.create_task(agent.run())
    await asyncio.sleep(0.01)
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task

    stop_calls = [payload for method, payload in runtime.calls if method == "agent.stop"]
    assert stop_calls == [{"run_id": agent.session_id}]


def test_agent_add_new_task_sends_followup_once() -> None:
    asyncio.run(_test_agent_add_new_task_sends_followup_once())


async def _test_agent_add_new_task_sends_followup_once() -> None:
    runtime = FakeRuntime(run_result={"history": {"output": "ok", "success": True}})
    agent = Agent("initial", browser=Browser(_runtime=runtime))  # type: ignore[arg-type]

    agent.add_new_task("next task")
    await agent.run()
    first_run = [payload for method, payload in runtime.calls if method == "agent.run"][-1]
    assert first_run["followups"] == ["next task"]

    await agent.run()
    second_run = [payload for method, payload in runtime.calls if method == "agent.run"][-1]
    assert "followups" not in second_run


def test_agent_run_fails_loudly_when_server_does_not_support_run() -> None:
    asyncio.run(_test_agent_run_fails_loudly_when_server_does_not_support_run())


async def _test_agent_run_fails_loudly_when_server_does_not_support_run() -> None:
    class CreateOnlyRuntime(FakeRuntime):
        async def call(
            self,
            method: str,
            params: Optional[Dict[str, Any]] = None,
        ) -> Dict[str, Any]:
            if method == "agent.run":
                raise JsonRpcError(-32601, "Method not found")
            return await super().call(method, params)

    agent = Agent("task", browser=Browser(_runtime=CreateOnlyRuntime()))  # type: ignore[arg-type]

    with pytest.raises(NotImplementedError, match="agent.run is not supported"):
        await agent.run()


def test_agent_run_propagates_real_backend_runtime_errors() -> None:
    asyncio.run(_test_agent_run_propagates_real_backend_runtime_errors())


async def _test_agent_run_propagates_real_backend_runtime_errors() -> None:
    class ProviderErrorRuntime(FakeRuntime):
        async def call(
            self,
            method: str,
            params: Optional[Dict[str, Any]] = None,
        ) -> Dict[str, Any]:
            if method == "agent.run":
                raise JsonRpcError(
                    -32000,
                    "no provider credentials found in environment",
                )
            return await super().call(method, params)

    agent = Agent("task", browser=Browser(_runtime=ProviderErrorRuntime()))  # type: ignore[arg-type]

    with pytest.raises(JsonRpcError, match="no provider credentials"):
        await agent.run()


def test_history_validates_structured_output_with_pydantic_v2_style() -> None:
    class Product:
        @classmethod
        def model_validate_json(cls, text: str) -> Dict[str, Any]:
            return json.loads(text)

    history = AgentHistoryList(output='{"name":"book"}', output_model_schema=Product)

    assert history.structured_output == {"name": "book"}


def test_runtime_client_round_trips_against_sdk_server_binary(tmp_path: Path) -> None:
    asyncio.run(_test_runtime_client_round_trips_against_sdk_server_binary(tmp_path))


async def _test_runtime_client_round_trips_against_sdk_server_binary(tmp_path: Path) -> None:
    binary = Path("target/debug/browser-use-terminal")
    if not binary.exists():
        pytest.skip("Rust SDK server binary is not built")
    source = Path("crates/browser-use-cli/src/main.rs")
    if source.exists() and binary.stat().st_mtime < source.stat().st_mtime:
        pytest.skip("Rust SDK server binary is stale")

    runtime = RuntimeClient(
        command=[
            str(binary),
            "--state-dir",
            str(tmp_path),
            "sdk-server",
            "--transport",
            "stdio",
        ]
    )
    try:
        assert await runtime.call("runtime.ping") == {
            "ok": True,
            "sdk_protocol_version": 1,
            "terminal_version": "0.1.8",
            "capabilities": [
                "browser.settings",
                "browser.set_backend",
                "browser.set_profile",
                "browser.create",
                "agent.run_task",
                "agent.run",
                "agent.stop",
                "runtime.python_worker_ping",
            ],
        }
        settings = await runtime.call("browser.set_backend", {"backend": "cloud"})
        assert settings["backend"] == "cloud"
        browser = await runtime.call("browser.create", {"headless": True})
        assert browser["browser_id"]
        agent = await runtime.call("agent.create", {"task": "inspect", "cwd": str(tmp_path)})
        assert agent["agent_id"]
        assert agent["session_id"]
        snapshot = await runtime.call("agent.snapshot", {"agent_id": agent["agent_id"]})
        assert snapshot["agent_id"] == agent["agent_id"]

        sdk_browser = Browser(_runtime=runtime)  # type: ignore[arg-type]
        sdk_agent = Agent(
            "inspect",
            llm=ChatBrowserUse(model="fake"),
            browser=sdk_browser,
        )
        history = await sdk_agent.run(max_steps=2)
        assert history.final_result() == "Fake result for: inspect"
        assert sdk_agent.session_id is not None
        queue = runtime.event_queue(sdk_agent.session_id)
        for _ in range(20):
            if not queue.empty():
                break
            await asyncio.sleep(0.01)
        observed = []
        while not queue.empty():
            observed.append(queue.get_nowait())
        assert observed
        assert any(event.get("kind") in {"agent_created", "store_event_appended"} for event in observed)
        projected_queue = runtime.projected_event_queue(sdk_agent.session_id)
        projected = []
        while not projected_queue.empty():
            projected.append(projected_queue.get_nowait())
        assert any(event.get("kind") == "thread_status_changed" for event in projected)
        assert not (tmp_path / "state.db").exists()
    finally:
        await runtime.close()
