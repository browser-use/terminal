from __future__ import annotations

import os
from pathlib import Path
from typing import Any, Dict, Optional, Type, Union

from .browser import Browser
from .history import AgentHistoryList
from .llm import ChatBrowserUse
from .runtime import RuntimeClient, default_runtime


class BrowserControl:
    def __init__(self, runtime: RuntimeClient) -> None:
        self._runtime = runtime

    async def settings(self) -> Dict[str, Any]:
        return await self._runtime.call("browser.settings", {})

    async def get_settings(self) -> Dict[str, Any]:
        return await self.settings()

    async def set_backend(
        self,
        backend: str,
        *,
        local_browser: Optional[str] = None,
        **params: Any,
    ) -> Dict[str, Any]:
        payload = dict(params)
        payload["backend"] = backend
        if local_browser is not None:
            payload["local_browser"] = local_browser
        return await self._runtime.call("browser.set_backend", payload)

    async def set_profile(
        self,
        profile_id: str,
        *,
        profile_label: Optional[str] = None,
        backend: Optional[str] = None,
        local_browser: Optional[str] = None,
        **params: Any,
    ) -> Dict[str, Any]:
        payload = dict(params)
        payload["profile_id"] = profile_id
        if profile_label is not None:
            payload["profile_label"] = profile_label
        if backend is not None:
            payload["backend"] = backend
        if local_browser is not None:
            payload["local_browser"] = local_browser
        return await self._runtime.call("browser.set_profile", payload)


class AgentControl:
    def __init__(self, runtime: RuntimeClient) -> None:
        self._runtime = runtime

    async def run(
        self,
        task: str,
        *,
        llm: Optional[ChatBrowserUse] = None,
        model: Optional[str] = None,
        max_steps: int = 100,
        browser: Optional[Union[Browser, Dict[str, Any]]] = None,
        cwd: Optional[Union[str, Path]] = None,
        output_model_schema: Optional[Type[Any]] = None,
        output_schema: Optional[Type[Any]] = None,
        use_vision: Any = True,
        max_actions_per_step: int = 5,
        **params: Any,
    ) -> AgentHistoryList:
        model_schema = output_model_schema or output_schema
        selected_llm = llm or ChatBrowserUse(model=model or "bu-2-0")
        payload = dict(params)
        payload.update(
            {
                "task": task,
                "cwd": str(cwd or os.getcwd()),
                "max_steps": max_steps,
                "llm": selected_llm.to_protocol(),
                "use_vision": use_vision,
                "max_actions_per_step": max_actions_per_step,
            }
        )
        if browser is not None:
            payload["browser"] = browser.to_protocol() if isinstance(browser, Browser) else browser
        if model_schema is not None:
            schema = _schema_for_model(model_schema)
            if schema is not None:
                payload["output_schema"] = schema
        result = await self._runtime.call("agent.run_task", payload)
        return AgentHistoryList.from_protocol(
            result or {},
            output_model_schema=model_schema,
        )

    async def run_task(self, task: str, **kwargs: Any) -> AgentHistoryList:
        return await self.run(task, **kwargs)


class Client:
    def __init__(
        self,
        *,
        state_dir: Optional[Union[str, Path]] = None,
        command: Optional[list[str]] = None,
        _runtime: Optional[RuntimeClient] = None,
    ) -> None:
        self.runtime = _runtime or (
            RuntimeClient(
                state_dir=Path(state_dir) if state_dir is not None else None,
                command=command,
            )
            if state_dir is not None or command is not None
            else default_runtime()
        )
        self.browser = BrowserControl(self.runtime)
        self.agent = AgentControl(self.runtime)

    async def close(self) -> None:
        await self.runtime.close()


BrowserUse = Client


def _schema_for_model(model: Type[Any]) -> Optional[Dict[str, Any]]:
    if hasattr(model, "model_json_schema"):
        return model.model_json_schema()
    if hasattr(model, "schema"):
        return model.schema()
    return None
