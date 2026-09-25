"""Async Python API for ACP-backed agent pools."""

from __future__ import annotations

import asyncio
import json
from typing import Any, Mapping, Optional, Union

from ._native import NativeAgentPool as _NativeAgentPool
from ._native import NativeTask as _NativeTask


def _encode_prompt(prompt: Union[str, Mapping[str, Any]]) -> str:
    if isinstance(prompt, str):
        value = {"content": [{"type": "text", "text": prompt}]}
    elif isinstance(prompt, Mapping) and isinstance(prompt.get("content"), list):
        value = dict(prompt)
    else:
        raise TypeError("prompt must be a string or an ACP prompt with a content list")
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


class Task:
    """One submitted task; result is awaitable and cancellation is immediate."""

    def __init__(self, native: _NativeTask) -> None:
        self._native = native

    @property
    def id(self) -> str:
        return self._native.id

    def cancel(self) -> None:
        self._native.cancel()

    async def result(self) -> dict[str, Any]:
        encoded = await asyncio.to_thread(self._native.result)
        return json.loads(encoded)


class AgentPool:
    """Bounded ACP worker pool with persistent, exclusive agent sessions."""

    def __init__(self, options: Mapping[str, Any]) -> None:
        if not isinstance(options, Mapping):
            raise TypeError("pool options must be a mapping")
        encoded = json.dumps(dict(options), ensure_ascii=False, separators=(",", ":"))
        self._native = _NativeAgentPool(encoded)

    def submit(
        self,
        prompt: Union[str, Mapping[str, Any]],
        agent_index: Optional[int] = None,
    ) -> Task:
        return Task(self._native.submit(_encode_prompt(prompt), agent_index))

    def submit_retrying(
        self,
        prompt: Union[str, Mapping[str, Any]],
        *,
        max_attempts: int,
        feedback_template: str,
        agent_index: Optional[int] = None,
    ) -> Task:
        if max_attempts < 1:
            raise ValueError("max_attempts must be greater than zero")
        if not feedback_template:
            raise ValueError("feedback_template must not be empty")
        return Task(
            self._native.submit_retrying(
                _encode_prompt(prompt), max_attempts, feedback_template, agent_index
            )
        )

    def status(self) -> dict[str, Any]:
        return json.loads(self._native.status_json())

    async def close(self, *, drain: bool = True) -> dict[str, Any]:
        encoded = await asyncio.to_thread(self._native.close, drain)
        return json.loads(encoded)

    async def __aenter__(self) -> "AgentPool":
        return self

    async def __aexit__(self, exc_type: Any, exc: Any, traceback: Any) -> None:
        await self.close(drain=exc_type is None)


__all__ = ["AgentPool", "Task"]
