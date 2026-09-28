"""Async Python API for ACP, Codex app-server, and Pi RPC agent pools."""

from __future__ import annotations

import asyncio
import json
from typing import Any, Mapping, Optional, Union

from ._native import NativeAgentPool as _NativeAgentPool
from ._native import NativeSessionLease as _NativeSessionLease


def _encode_prompt(prompt: Union[str, Mapping[str, Any]]) -> str:
    if isinstance(prompt, str):
        value = {"content": [{"type": "text", "text": prompt}]}
    elif isinstance(prompt, Mapping) and isinstance(prompt.get("content"), list):
        value = dict(prompt)
    else:
        raise TypeError("prompt must be a string or a prompt with a content list")
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


class SessionLease:
    """Exclusive agent session until ``finish`` or context exit."""

    def __init__(self, native: _NativeSessionLease) -> None:
        self._native: Optional[_NativeSessionLease] = native
        self.agent_index = native.agent_index

    async def ask(self, prompt: Union[str, Mapping[str, Any]]) -> dict[str, Any]:
        native = self._native
        if native is None:
            raise RuntimeError("session lease is finished")
        encoded = await asyncio.to_thread(native.ask, _encode_prompt(prompt))
        return json.loads(encoded)

    async def finish(self) -> None:
        native = self._native
        if native is not None:
            self._native = None
            await asyncio.to_thread(native.finish)

    async def __aenter__(self) -> "SessionLease":
        return self

    async def __aexit__(self, exc_type: Any, exc: Any, traceback: Any) -> None:
        await self.finish()


class AgentPool:
    """Bounded worker pool with persistent, exclusive agent sessions."""

    def __init__(self, options: Mapping[str, Any]) -> None:
        if not isinstance(options, Mapping):
            raise TypeError("pool options must be a mapping")
        encoded = json.dumps(dict(options), ensure_ascii=False, separators=(",", ":"))
        self._native = _NativeAgentPool(encoded)

    async def acquire(self, agent_index: Optional[int] = None) -> SessionLease:
        pending = self._native.request_lease(agent_index)
        try:
            native = await asyncio.to_thread(pending.wait)
        except asyncio.CancelledError:
            pending.cancel()
            raise
        return SessionLease(native)

    def status(self) -> dict[str, Any]:
        return json.loads(self._native.status_json())

    async def close(self, *, drain: bool = True) -> dict[str, Any]:
        encoded = await asyncio.to_thread(self._native.close, drain)
        return json.loads(encoded)

    async def __aenter__(self) -> "AgentPool":
        return self

    async def __aexit__(self, exc_type: Any, exc: Any, traceback: Any) -> None:
        await self.close(drain=exc_type is None)


__all__ = ["AgentPool", "SessionLease"]
