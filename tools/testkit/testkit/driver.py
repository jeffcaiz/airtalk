"""Spawn airtalk-core, feed it NDJSON, collect the terminal response."""

from __future__ import annotations

import asyncio
import base64
import json
import os
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from pydub import AudioSegment


@dataclass
class CoreRun:
    response: Optional[dict]
    exit_code: int
    stderr: str
    vad_segments: Optional[int]  # parsed from stderr "segments_emitted=N"
    elapsed_s: float


async def run_core(
    wav_path: Path,
    *,
    core_bin: Path,
    vad: bool = True,
    no_llm: bool = True,
    llm_base_url: Optional[str] = None,
    llm_model: Optional[str] = None,
    context: Optional[str] = None,
    language: Optional[str] = None,
    timeout_s: float = 120.0,
) -> CoreRun:
    """Drive a single begin/chunk/end session through core, return its
    terminal response plus parsed diagnostics from stderr.

    When `no_llm=False`, `llm_base_url` and `llm_model` are required.
    The LLM API key must be in env as `AIRTALK_LLM_API_KEY`.
    """
    audio = AudioSegment.from_wav(wav_path)
    audio = audio.set_frame_rate(16000).set_channels(1).set_sample_width(2)
    pcm_bytes = audio.raw_data

    args = [str(core_bin)]
    if no_llm:
        args.append("--no-llm")
    else:
        if not llm_base_url or not llm_model:
            raise ValueError(
                "llm_base_url and llm_model are required when no_llm=False"
            )
        args.extend(
            ["--llm-base-url", llm_base_url, "--llm-model", llm_model]
        )
    args.extend(["--log-level", "info"])

    env = dict(os.environ)
    if "AIRTALK_ASR_API_KEY" not in env:
        raise RuntimeError(
            "AIRTALK_ASR_API_KEY is not set; testkit needs a real DashScope key "
            "since ASR is called end-to-end"
        )
    if not no_llm and "AIRTALK_LLM_API_KEY" not in env:
        raise RuntimeError(
            "AIRTALK_LLM_API_KEY is not set but LLM cleanup was requested"
        )

    proc = await asyncio.create_subprocess_exec(
        *args,
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env=env,
    )

    begin: dict = {"type": "begin", "id": 1, "vad": vad}
    if context is not None:
        begin["context"] = context
    if language is not None:
        begin["language"] = language
    chunk = {"type": "chunk", "id": 1, "pcm": base64.b64encode(pcm_bytes).decode()}
    end = {"type": "end", "id": 1}

    payload = (
        json.dumps(begin) + "\n" + json.dumps(chunk) + "\n" + json.dumps(end) + "\n"
    )

    started = asyncio.get_event_loop().time()

    async def _pump() -> tuple[Optional[dict], bytes, bytes]:
        assert proc.stdin is not None
        proc.stdin.write(payload.encode())
        await proc.stdin.drain()
        proc.stdin.close()
        stdout_bytes, stderr_bytes = await proc.communicate()
        terminal: Optional[dict] = None
        for raw_line in stdout_bytes.splitlines():
            line = raw_line.decode("utf-8", errors="replace").strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            kind = msg.get("type")
            if kind in ("result", "error") and msg.get("id") == 1:
                terminal = msg
        return terminal, stdout_bytes, stderr_bytes

    try:
        terminal, _stdout, stderr = await asyncio.wait_for(_pump(), timeout=timeout_s)
    except asyncio.TimeoutError:
        proc.kill()
        await proc.wait()
        return CoreRun(
            response=None,
            exit_code=-1,
            stderr=f"<timeout after {timeout_s}s>",
            vad_segments=None,
            elapsed_s=timeout_s,
        )

    elapsed = asyncio.get_event_loop().time() - started
    exit_code = proc.returncode if proc.returncode is not None else -1
    stderr_str = stderr.decode("utf-8", errors="replace")
    vad_segments = _parse_segments(stderr_str)

    return CoreRun(
        response=terminal,
        exit_code=exit_code,
        stderr=stderr_str,
        vad_segments=vad_segments,
        elapsed_s=elapsed,
    )


_SEGMENTS_RE = re.compile(r"segments_emitted=(\d+)")


def _parse_segments(stderr: str) -> Optional[int]:
    match = _SEGMENTS_RE.search(stderr)
    if match:
        return int(match.group(1))
    return None
