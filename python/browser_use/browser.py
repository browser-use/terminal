from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any, Dict, List, Optional

from .exceptions import BrowserAlreadyInUseError
from .runtime import RuntimeClient, default_runtime


class BrowserProfile:
    def __init__(
        self,
        *,
        allowed_domains: Optional[List[str]] = None,
        blocked_domains: Optional[List[str]] = None,
        keep_alive: Optional[bool] = None,
        profile_id: Optional[str] = None,
        state_dir: Optional[Path] = None,
        **options: Any,
    ) -> None:
        self.allowed_domains = list(allowed_domains or [])
        self.blocked_domains = list(blocked_domains or [])
        self.keep_alive = keep_alive
        self.profile_id = profile_id
        self.state_dir = Path(state_dir) if state_dir is not None else None
        self.options = dict(options)

    def to_protocol(self) -> Dict[str, Any]:
        payload = dict(self.options)
        if self.allowed_domains:
            payload["allowed_domains"] = self.allowed_domains
        if self.blocked_domains:
            payload["blocked_domains"] = self.blocked_domains
        if self.keep_alive is not None:
            payload["keep_alive"] = self.keep_alive
        if self.profile_id is not None:
            payload["profile_id"] = self.profile_id
        if self.state_dir is not None:
            payload["state_dir"] = str(self.state_dir)
        return payload


class Browser:
    def __init__(
        self,
        *,
        headless: Optional[bool] = None,
        keep_alive: Optional[bool] = None,
        proxy_country_code: Optional[str] = None,
        profile: Optional[str] = None,
        profile_id: Optional[str] = None,
        cdp_url: Optional[str] = None,
        cdp_headers: Optional[Any] = None,
        storage_state: Optional[Any] = None,
        user_agent: Optional[str] = None,
        allowed_domains: Optional[List[str]] = None,
        blocked_domains: Optional[List[str]] = None,
        viewport: Optional[Dict[str, Any]] = None,
        window_size: Optional[Dict[str, Any]] = None,
        downloads_path: Optional[Path] = None,
        state_dir: Optional[Path] = None,
        no_viewport: Optional[bool] = None,
        accept_downloads: Optional[bool] = None,
        _runtime: Optional[RuntimeClient] = None,
    ) -> None:
        self.headless = headless
        self.keep_alive = bool(keep_alive) if keep_alive is not None else False
        self.proxy_country_code = proxy_country_code
        self.profile = profile
        self.profile_id = profile_id
        self.cdp_url = cdp_url
        self.cdp_headers = cdp_headers
        self.storage_state = storage_state
        self.user_agent = user_agent
        self.allowed_domains = list(allowed_domains or [])
        self.blocked_domains = list(blocked_domains or [])
        self.viewport = dict(viewport or {})
        self.window_size = dict(window_size or {})
        self.downloads_path = Path(downloads_path) if downloads_path is not None else None
        self.state_dir = Path(state_dir) if state_dir is not None else None
        self.no_viewport = no_viewport
        self.accept_downloads = accept_downloads
        self._runtime = _runtime or default_runtime()
        self.browser_id: Optional[str] = None
        self._active_owner: Optional[object] = None
        self._lock = asyncio.Lock()

    @property
    def runtime(self) -> RuntimeClient:
        return self._runtime

    async def start(self) -> None:
        if self.browser_id is not None:
            return
        result = await self._runtime.call("browser.create", self.to_protocol())
        self.browser_id = str(result["browser_id"])

    async def stop(self) -> None:
        if self.browser_id is None:
            return
        try:
            await self._runtime.call("browser.stop", {"browser_id": self.browser_id})
        except Exception:
            pass
        self.browser_id = None

    async def close(self) -> None:
        if self.browser_id is None:
            return
        try:
            await self._runtime.call("browser.close", {"browser_id": self.browser_id})
        except Exception:
            pass
        self.browser_id = None

    async def kill(self) -> None:
        await self.close()

    def to_protocol(self) -> Dict[str, Any]:
        payload: Dict[str, Any] = {
            "keep_alive": self.keep_alive,
        }
        if self.headless is not None:
            payload["headless"] = self.headless
        if self.proxy_country_code is not None:
            payload["proxy_country_code"] = self.proxy_country_code
        if self.profile is not None:
            payload["profile"] = self.profile
        if self.profile_id is not None:
            payload["profile_id"] = self.profile_id
        if self.cdp_url is not None:
            payload["cdp_url"] = self.cdp_url
        if self.cdp_headers is not None:
            payload["cdp_headers"] = self.cdp_headers
        if self.storage_state is not None:
            payload["storage_state"] = self.storage_state
        if self.user_agent is not None:
            payload["user_agent"] = self.user_agent
        if self.allowed_domains:
            payload["allowed_domains"] = self.allowed_domains
        if self.blocked_domains:
            payload["blocked_domains"] = self.blocked_domains
        if self.viewport:
            payload["viewport"] = self.viewport
        if self.window_size:
            payload["window_size"] = self.window_size
        if self.downloads_path is not None:
            payload["downloads_path"] = str(self.downloads_path)
        if self.state_dir is not None:
            payload["state_dir"] = str(self.state_dir)
        if self.no_viewport is not None:
            payload["no_viewport"] = self.no_viewport
        if self.accept_downloads is not None:
            payload["accept_downloads"] = self.accept_downloads
        return payload

    async def _claim(self, owner: object) -> None:
        async with self._lock:
            if self._active_owner is not None and self._active_owner is not owner:
                raise BrowserAlreadyInUseError(
                    "this Browser is already attached to a running Agent"
                )
            self._active_owner = owner

    async def _release(self, owner: object) -> None:
        async with self._lock:
            if self._active_owner is owner:
                self._active_owner = None


BrowserSession = Browser
