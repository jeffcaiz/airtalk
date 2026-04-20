"""Persistent `airtalk-core` subprocess wrapper.

Unlike `driver.run_core` — which spawns a fresh core for one batch
session — `CoreSession` keeps a single core alive across many
begin/chunk/end rounds. Callers do streaming chunks and read terminal
responses as they arrive, matching how the real UI will drive core.
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
from pathlib import Path
from typing import Iterable, Optional


class CoreSession:
    """One long-lived airtalk-core child process.

    Assumes a strictly serial round pattern: at most one `begin` session
    in flight at a time, the caller `recv_terminal`s its response
    before starting the next. No supersede bookkeeping on the client
    side; core's own supersede behavior is out of scope here.
    """

    def __init__(self, proc: asyncio.subprocess.Process) -> None:
        self._proc = proc
        self._write_lock = asyncio.Lock()
        self._next_id = 0
        self._stderr_task: Optional[asyncio.Task] = None

    @classmethod
    async def spawn(
        cls,
        *,
        core_bin: Path,
        no_llm: bool,
        llm_base_url: Optional[str] = None,
        llm_model: Optional[str] = None,
        log_level: str = "info",
    ) -> "CoreSession":
        args: list[str] = [str(core_bin)]
        if no_llm:
            args.append("--no-llm")
        else:
            if not llm_base_url or not llm_model:
                raise ValueError(
                    "llm_base_url + llm_model required when no_llm=False"
                )
            args += ["--llm-base-url", llm_base_url, "--llm-model", llm_model]
        args += ["--log-level", log_level]

        env = dict(os.environ)
        if "AIRTALK_ASR_API_KEY" not in env:
            raise RuntimeError(
                "AIRTALK_ASR_API_KEY must be set (core calls DashScope end-to-end)"
            )
        if not no_llm and "AIRTALK_LLM_API_KEY" not in env:
            raise RuntimeError(
                "AIRTALK_LLM_API_KEY must be set unless --no-llm is passed"
            )

        proc = await asyncio.create_subprocess_exec(
            *args,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=env,
        )

        session = cls(proc)
        # Drain stderr in the background: the pipe has a finite buffer,
        # and core's info-level logging would eventually block writes
        # if nothing reads it. We also tee it to our own stderr so the
        # user sees VAD summaries and warnings.
        session._stderr_task = asyncio.create_task(session._pump_stderr())

        # Read stdout until we see Ready. Skip any other lines (there
        # shouldn't be any before Ready, but be defensive).
        assert proc.stdout is not None
        while True:
            line = await proc.stdout.readline()
            if not line:
                raise RuntimeError("core exited before emitting Ready")
            try:
                msg = json.loads(line.decode("utf-8").strip())
            except json.JSONDecodeError:
                continue
            if msg.get("type") == "ready":
                return session

    async def _pump_stderr(self) -> None:
        assert self._proc.stderr is not None
        while True:
            line = await self._proc.stderr.readline()
            if not line:
                return
            sys.stderr.write(line.decode("utf-8", errors="replace"))
            sys.stderr.flush()

    def next_id(self) -> int:
        self._next_id += 1
        return self._next_id

    async def send(self, frame: dict) -> None:
        assert self._proc.stdin is not None
        payload = (json.dumps(frame, ensure_ascii=False) + "\n").encode("utf-8")
        async with self._write_lock:
            self._proc.stdin.write(payload)
            await self._proc.stdin.drain()

    async def recv_terminal(
        self,
        session_id: int,
        allowed: Iterable[str] = ("result", "error"),
    ) -> dict:
        """Read stdout until a response for `session_id` with a type in
        `allowed` arrives. Stale responses for other ids are dropped."""
        assert self._proc.stdout is not None
        allowed_set = set(allowed)
        while True:
            line = await self._proc.stdout.readline()
            if not line:
                raise RuntimeError("core stdout closed before terminal response")
            try:
                msg = json.loads(line.decode("utf-8").strip())
            except json.JSONDecodeError:
                continue
            if msg.get("id") == session_id and msg.get("type") in allowed_set:
                return msg

    async def close(self, timeout_s: float = 3.0) -> int:
        """Close stdin and wait for clean exit. Kills if exit is too slow."""
        if self._proc.stdin is not None and not self._proc.stdin.is_closing():
            self._proc.stdin.close()
        try:
            await asyncio.wait_for(self._proc.wait(), timeout=timeout_s)
        except asyncio.TimeoutError:
            self._proc.kill()
            await self._proc.wait()
        if self._stderr_task is not None:
            self._stderr_task.cancel()
        return self._proc.returncode if self._proc.returncode is not None else 0
