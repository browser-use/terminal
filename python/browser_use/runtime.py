from __future__ import annotations

import asyncio
import json
import os
import shutil
from pathlib import Path
from typing import Any, Dict, List, Optional

from .exceptions import BrowserUseProtocolError, BrowserUseRuntimeError


class JsonRpcError(BrowserUseRuntimeError):
    def __init__(self, code: int, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


class RuntimeClient:
    def __init__(
        self,
        *,
        state_dir: Optional[Path] = None,
        command: Optional[List[str]] = None,
    ) -> None:
        self.state_dir = Path(state_dir) if state_dir is not None else None
        self.command = list(command) if command is not None else None
        self._process: Optional[asyncio.subprocess.Process] = None
        self._reader_task: Optional[asyncio.Task[Any]] = None
        self._stderr_task: Optional[asyncio.Task[Any]] = None
        self._next_id = 1
        self._write_lock = asyncio.Lock()
        self._pending: Dict[int, asyncio.Future[Any]] = {}
        self._events: Dict[str, asyncio.Queue[Dict[str, Any]]] = {}
        self._projected_events: Dict[str, asyncio.Queue[Dict[str, Any]]] = {}
        self.stderr_lines: List[str] = []

    async def call(self, method: str, params: Optional[Dict[str, Any]] = None) -> Any:
        await self.start()
        if self._process is None or self._process.stdin is None:
            raise BrowserUseProtocolError("Rust SDK server stdin is unavailable")

        loop = asyncio.get_running_loop()
        async with self._write_lock:
            request_id = self._next_id
            self._next_id += 1
            future: asyncio.Future[Any] = loop.create_future()
            self._pending[request_id] = future

            request = {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params or {},
            }
            self._process.stdin.write((json.dumps(request) + "\n").encode("utf-8"))
            await self._process.stdin.drain()
        try:
            return await future
        except asyncio.CancelledError:
            self._pending.pop(request_id, None)
            raise

    async def notify(self, method: str, params: Optional[Dict[str, Any]] = None) -> None:
        await self.start()
        if self._process is None or self._process.stdin is None:
            raise BrowserUseProtocolError("Rust SDK server stdin is unavailable")
        request = {"jsonrpc": "2.0", "method": method, "params": params or {}}
        async with self._write_lock:
            self._process.stdin.write((json.dumps(request) + "\n").encode("utf-8"))
            await self._process.stdin.drain()

    async def start(self) -> None:
        if self._process is not None and self._process.returncode is None:
            return
        command = self.command or _default_sdk_server_command(self.state_dir)
        self._process = await asyncio.create_subprocess_exec(
            *command,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        self._reader_task = asyncio.create_task(self._read_stdout())
        self._stderr_task = asyncio.create_task(self._read_stderr())

    async def close(self) -> None:
        process = self._process
        if process is None:
            return
        if process.stdin is not None:
            process.stdin.close()
            try:
                await process.stdin.wait_closed()
            except (BrokenPipeError, ConnectionResetError):
                pass
        if process.returncode is None:
            process.terminate()
            try:
                await asyncio.wait_for(process.wait(), timeout=2)
            except asyncio.TimeoutError:
                process.kill()
                await process.wait()
        for task in (self._reader_task, self._stderr_task):
            if task is not None:
                task.cancel()
        self._fail_all(BrowserUseProtocolError("Rust SDK server closed"))
        self._process = None

    def event_queue(self, run_id: str) -> asyncio.Queue[Dict[str, Any]]:
        return self._events.setdefault(run_id, asyncio.Queue())

    def projected_event_queue(self, run_id: str) -> asyncio.Queue[Dict[str, Any]]:
        return self._projected_events.setdefault(run_id, asyncio.Queue())

    async def _read_stdout(self) -> None:
        assert self._process is not None
        assert self._process.stdout is not None
        async for raw_line in self._process.stdout:
            line = raw_line.decode("utf-8", errors="replace").strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except json.JSONDecodeError as error:
                self._fail_all(BrowserUseProtocolError(f"invalid JSON-RPC line: {line}: {error}"))
                return
            self._handle_message(message)

    async def _read_stderr(self) -> None:
        assert self._process is not None
        assert self._process.stderr is not None
        async for raw_line in self._process.stderr:
            self.stderr_lines.append(raw_line.decode("utf-8", errors="replace").rstrip())
            del self.stderr_lines[:-200]

    def _handle_message(self, message: Dict[str, Any]) -> None:
        if "id" in message:
            request_id = message.get("id")
            if not isinstance(request_id, int):
                self._fail_all(
                    BrowserUseProtocolError("JSON-RPC response id must be an integer")
                )
                return
            future = self._pending.pop(request_id, None)
            if future is None or future.done():
                return
            if "error" in message:
                error = message["error"] or {}
                future.set_exception(
                    JsonRpcError(
                        int(error.get("code", -32000)),
                        str(error.get("message", "runtime error")),
                    )
                )
            else:
                future.set_result(message.get("result"))
            return

        if message.get("method") == "agent.event":
            params = message.get("params") or {}
            run_id = params.get("run_id")
            session_id = params.get("session_id")
            event = params.get("event") or {}
            if isinstance(run_id, str):
                self.event_queue(run_id).put_nowait(event)
            if isinstance(session_id, str) and session_id != run_id:
                self.event_queue(session_id).put_nowait(event)
            return

        if message.get("method") == "agent.projected_event":
            params = message.get("params") or {}
            run_id = params.get("run_id")
            session_id = params.get("session_id")
            event = params.get("event") or {}
            if isinstance(run_id, str):
                self.projected_event_queue(run_id).put_nowait(event)
            if isinstance(session_id, str) and session_id != run_id:
                self.projected_event_queue(session_id).put_nowait(event)

    def _fail_all(self, error: BaseException) -> None:
        for future in self._pending.values():
            if not future.done():
                future.set_exception(error)
        self._pending.clear()


_default_runtime: Optional[RuntimeClient] = None


def default_runtime() -> RuntimeClient:
    global _default_runtime
    if _default_runtime is None:
        _default_runtime = RuntimeClient()
    return _default_runtime


def _default_sdk_server_command(state_dir: Optional[Path]) -> List[str]:
    explicit = os.environ.get("BROWSER_USE_SDK_SERVER")
    if explicit:
        command = [explicit]
    else:
        binary = shutil.which("browser-use-terminal")
        if binary:
            command = [binary]
        else:
            repo_root = Path(__file__).resolve().parents[2]
            debug_binary = repo_root / "target" / "debug" / "browser-use-terminal"
            if debug_binary.exists():
                command = [str(debug_binary)]
            else:
                command = ["cargo", "run", "-q", "-p", "browser-use-cli", "--"]

    if state_dir is not None:
        command.extend(["--state-dir", str(state_dir)])
    command.extend(["sdk-server", "--transport", "stdio"])
    return command
