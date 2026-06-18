from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any, Dict, Iterable, List, Optional, Type


@dataclass
class ActionResult:
    action: str
    result: Any = None
    error: Optional[str] = None
    metadata: Dict[str, Any] = field(default_factory=dict)


class AgentHistoryList:
    def __init__(
        self,
        *,
        output: Any = None,
        events: Optional[Iterable[Dict[str, Any]]] = None,
        actions: Optional[Iterable[ActionResult]] = None,
        errors: Optional[Iterable[str]] = None,
        success: bool = True,
        done: bool = True,
        output_model_schema: Optional[Type[Any]] = None,
    ) -> None:
        self._output = output
        self.events = list(events or [])
        self.actions = list(actions or [])
        self._errors = list(errors or [])
        self._success = success
        self._done = done
        self._output_model_schema = output_model_schema
        self._structured_output: Any = _missing

    @classmethod
    def from_protocol(
        cls,
        payload: Dict[str, Any],
        *,
        output_model_schema: Optional[Type[Any]] = None,
    ) -> "AgentHistoryList":
        history = payload.get("history") if isinstance(payload, dict) else None
        source = history if isinstance(history, dict) else payload

        raw_actions = source.get("actions") or []
        actions = []
        for item in raw_actions:
            if isinstance(item, ActionResult):
                actions.append(item)
            elif isinstance(item, dict):
                actions.append(
                    ActionResult(
                        action=str(item.get("action") or item.get("name") or ""),
                        result=item.get("result"),
                        error=item.get("error"),
                        metadata=dict(item.get("metadata") or {}),
                    )
                )

        return cls(
            output=source.get("output", source.get("final_result")),
            events=source.get("events") or [],
            actions=actions,
            errors=source.get("errors") or [],
            success=bool(source.get("success", source.get("is_successful", True))),
            done=bool(source.get("done", source.get("is_done", True))),
            output_model_schema=output_model_schema,
        )

    def final_result(self) -> Any:
        return self._output

    @property
    def output(self) -> Any:
        return self.final_result()

    @property
    def structured_output(self) -> Any:
        if self._structured_output is _missing:
            self._structured_output = self.get_structured_output(self._output_model_schema)
        return self._structured_output

    def get_structured_output(self, model: Optional[Type[Any]] = None) -> Any:
        if model is None:
            return None
        output = self.final_result()
        if output is None:
            return None

        if hasattr(model, "model_validate_json") and isinstance(output, str):
            return model.model_validate_json(output)
        if hasattr(model, "model_validate"):
            if isinstance(output, str):
                try:
                    output = json.loads(output)
                except json.JSONDecodeError:
                    pass
            return model.model_validate(output)
        if hasattr(model, "parse_raw") and isinstance(output, str):
            return model.parse_raw(output)
        if hasattr(model, "parse_obj"):
            if isinstance(output, str):
                output = json.loads(output)
            return model.parse_obj(output)
        return model(output)

    def is_done(self) -> bool:
        return self._done

    def is_successful(self) -> bool:
        return self._success and not self._errors

    def errors(self) -> List[str]:
        errors = list(self._errors)
        for action in self.actions:
            if action.error:
                errors.append(action.error)
        return errors

    def action_names(self) -> List[str]:
        return [action.action for action in self.actions if action.action]

    def model_actions(self) -> List[ActionResult]:
        return list(self.actions)


class _Missing:
    pass


_missing = _Missing()
