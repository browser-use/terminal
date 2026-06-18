from __future__ import annotations

from typing import Any, Dict, Optional


class ChatBrowserUse:
    provider = "browser-use"

    def __init__(
        self,
        model: str = "bu-2-0",
        *,
        api_key: Optional[str] = None,
        base_url: Optional[str] = None,
        timeout: float = 120.0,
        **options: Any,
    ) -> None:
        self.model = model
        self.api_key = api_key
        self.base_url = base_url
        self.timeout = timeout
        self.options = dict(options)

    def to_protocol(self) -> Dict[str, Any]:
        payload: Dict[str, Any] = {
            "provider": self.provider,
            "model": self.model,
            "timeout": self.timeout,
        }
        if self.api_key is not None:
            payload["api_key"] = self.api_key
        if self.base_url is not None:
            payload["base_url"] = self.base_url
        if self.options:
            payload["options"] = self.options
        return payload
