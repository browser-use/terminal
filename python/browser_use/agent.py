from __future__ import annotations

import asyncio
import os
from typing import Any, AsyncIterator, Dict, List, Optional, Type

from .browser import Browser, BrowserProfile
from .exceptions import BrowserAlreadyInUseError
from .history import AgentHistoryList
from .llm import ChatBrowserUse
from .runtime import JsonRpcError


class Agent:
    def __init__(
        self,
        task: str,
        llm: Optional[ChatBrowserUse] = None,
        *,
        browser: Optional[Browser] = None,
        browser_session: Optional[Browser] = None,
        browser_profile: Optional[BrowserProfile] = None,
        output_model_schema: Optional[Type[Any]] = None,
        output_schema: Optional[Type[Any]] = None,
        sensitive_data: Optional[Dict[str, Any]] = None,
        use_vision: Any = True,
        max_actions_per_step: int = 5,
        tools: Optional[object] = None,
        **kwargs: Any,
    ) -> None:
        if tools is not None:
            raise NotImplementedError("custom Tools are not supported by this runtime yet")
        if browser is not None and browser_session is not None and browser is not browser_session:
            raise ValueError("pass only one of browser or browser_session")

        self.task = task
        self.llm = llm or ChatBrowserUse()
        selected_browser = browser or browser_session
        if selected_browser is None:
            selected_browser = Browser(
                keep_alive=browser_profile.keep_alive if browser_profile else None,
                profile_id=browser_profile.profile_id if browser_profile else None,
                allowed_domains=browser_profile.allowed_domains if browser_profile else None,
                blocked_domains=browser_profile.blocked_domains if browser_profile else None,
                state_dir=browser_profile.state_dir if browser_profile else None,
            )
        self.browser = selected_browser
        self.browser_profile = browser_profile
        self.output_model_schema = output_model_schema or output_schema
        self.sensitive_data = dict(sensitive_data or {})
        self.use_vision = use_vision
        self.max_actions_per_step = max_actions_per_step
        self.extra_options = dict(kwargs)
        self.agent_id: Optional[str] = None
        self.session_id: Optional[str] = None
        self._pending_tasks: List[str] = []
        self._active_run_id: Optional[str] = None
        self._stopped = False

    async def run(self, max_steps: int = 100) -> AgentHistoryList:
        await self.browser._claim(self)
        try:
            await self.browser.start()
            await self._ensure_created()
            params = self._run_params(max_steps)

            try:
                self._active_run_id = self.session_id
                result = await self.browser.runtime.call("agent.run", params)
            except JsonRpcError as error:
                if error.code == -32601:
                    raise NotImplementedError(
                        "agent.run is not supported by this Rust SDK server yet"
                    ) from error
                if "already" in error.message.lower() and "browser" in error.message.lower():
                    raise BrowserAlreadyInUseError(error.message) from error
                raise
            return AgentHistoryList.from_protocol(
                result or {},
                output_model_schema=self.output_model_schema,
            )
        except asyncio.CancelledError:
            await self._cancel_active_run()
            raise
        finally:
            self._active_run_id = None
            await self.browser._release(self)

    async def stream(self, max_steps: int = 100) -> AsyncIterator[Dict[str, Any]]:
        await self.browser._claim(self)
        try:
            await self.browser.start()
            await self._ensure_created()
            params = self._run_params(max_steps)
            assert self.session_id is not None
            queue = self.browser.runtime.projected_event_queue(self.session_id)
            snapshot = await self.browser.runtime.call(
                "agent.snapshot", {"agent_id": self.agent_id}
            )
            yield {"type": "agent.snapshot", "snapshot": snapshot}
            run_task = asyncio.create_task(self.browser.runtime.call("agent.run", params))
            self._active_run_id = self.session_id
            saw_terminal_projection = False
            try:
                while True:
                    event_task = asyncio.create_task(queue.get())
                    done, pending = await asyncio.wait(
                        {run_task, event_task},
                        return_when=asyncio.FIRST_COMPLETED,
                    )
                    if event_task in done:
                        event = event_task.result()
                        saw_terminal_projection = (
                            saw_terminal_projection or _is_terminal_projected_event(event)
                        )
                        yield event
                    else:
                        event_task.cancel()
                    if run_task in done:
                        for task in pending:
                            task.cancel()
                        result = run_task.result()
                        for event in _drain_queue(queue):
                            saw_terminal_projection = (
                                saw_terminal_projection
                                or _is_terminal_projected_event(event)
                            )
                            yield event
                        try:
                            event = await asyncio.wait_for(queue.get(), timeout=0.05)
                            saw_terminal_projection = (
                                saw_terminal_projection
                                or _is_terminal_projected_event(event)
                            )
                            yield event
                            for event in _drain_queue(queue):
                                saw_terminal_projection = (
                                    saw_terminal_projection
                                    or _is_terminal_projected_event(event)
                                )
                                yield event
                        except asyncio.TimeoutError:
                            pass
                        final_projected = (result or {}).get("final_projected_event")
                        if (
                            isinstance(final_projected, dict)
                            and not saw_terminal_projection
                        ):
                            yield final_projected
                        history = AgentHistoryList.from_protocol(
                            result or {},
                            output_model_schema=self.output_model_schema,
                        )
                        yield {
                            "type": "agent.completed",
                            "history": history,
                            "output": history.final_result(),
                            "success": history.is_successful(),
                        }
                        return
            except asyncio.CancelledError:
                await self._cancel_active_run()
                run_task.cancel()
                raise
            finally:
                self._active_run_id = None
        finally:
            await self.browser._release(self)

    def add_new_task(self, new_task: str) -> None:
        self._pending_tasks.append(new_task)

    def stop(self) -> None:
        self._stopped = True
        if self._active_run_id is None:
            return
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            return
        loop.create_task(self._cancel_active_run())

    async def close(self) -> None:
        if self.agent_id is not None:
            try:
                await self.browser.runtime.call("agent.close", {"agent_id": self.agent_id})
            except JsonRpcError:
                pass
        if not self.browser.keep_alive:
            await self.browser.close()

    async def _ensure_created(self) -> None:
        if self.agent_id is not None:
            return
        params = {
            "task": self.task,
            "cwd": os.getcwd(),
            "llm": self.llm.to_protocol(),
        }
        if self.browser.browser_id is not None:
            params["browser_id"] = self.browser.browser_id
        result = await self.browser.runtime.call("agent.create", params)
        self.agent_id = str(result["agent_id"])
        self.session_id = str(result["session_id"])

    def _run_params(self, max_steps: int) -> Dict[str, Any]:
        params: Dict[str, Any] = {
            "agent_id": self.agent_id,
            "browser_id": self.browser.browser_id,
            "max_steps": max_steps,
            "llm": self.llm.to_protocol(),
            "use_vision": self.use_vision,
            "max_actions_per_step": self.max_actions_per_step,
        }
        if self._pending_tasks:
            params["followups"] = list(self._pending_tasks)
            self._pending_tasks.clear()
        if self.output_model_schema is not None:
            params["output_schema"] = _schema_for_model(self.output_model_schema)
        return params

    async def _cancel_active_run(self) -> None:
        if self._active_run_id is None:
            return
        try:
            await self.browser.runtime.call("agent.stop", {"run_id": self._active_run_id})
        except JsonRpcError:
            pass


def _schema_for_model(model: Type[Any]) -> Optional[Dict[str, Any]]:
    if hasattr(model, "model_json_schema"):
        return model.model_json_schema()
    if hasattr(model, "schema"):
        return model.schema()
    return None


def _drain_queue(queue: asyncio.Queue[Dict[str, Any]]) -> List[Dict[str, Any]]:
    drained: List[Dict[str, Any]] = []
    while True:
        try:
            drained.append(queue.get_nowait())
        except asyncio.QueueEmpty:
            return drained


def _is_terminal_projected_event(event: Dict[str, Any]) -> bool:
    kind = event.get("kind")
    if kind in {"turn_completed", "thread_status_changed"}:
        snapshot = event.get("snapshot")
        if isinstance(snapshot, dict):
            agents = snapshot.get("agents")
            if isinstance(agents, list):
                for agent in agents:
                    if not isinstance(agent, dict):
                        continue
                    if agent.get("status") in {
                        "completed",
                        "failed",
                        "cancelled",
                        "closed",
                    }:
                        return True
        payload = event.get("payload")
        if isinstance(payload, dict) and (
            "result" in payload or "error" in payload or "success" in payload
        ):
            return True
    return False
