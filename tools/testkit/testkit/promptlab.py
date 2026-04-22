"""Offline LLM cleanup iteration harness.

Reads a YAML fixture of (name, raw, assertions) cases and a system
prompt file, then hits the OpenAI-compatible chat endpoint for each
case in parallel and prints raw vs. cleaned output plus assertion
results.

Goal: iterate on the cleanup prompt without re-recording audio or
rebuilding airtalk-core. Accumulate a regression fixture as you find
new failure modes.

Request shape mirrors `airtalk-core/src/llm/openai.rs` so results
match what core would produce in production (same temperature,
`enable_thinking=false`, etc.).
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

import yaml


@dataclass
class Case:
    name: str
    raw: str
    must_contain: list[str]
    must_not_contain: list[str]
    note: Optional[str]


@dataclass
class Result:
    case: Case
    cleaned: Optional[str]
    latency_ms: float
    usage: Optional[dict]
    error: Optional[str]


def load_cases(path: Path) -> list[Case]:
    data = yaml.safe_load(path.read_text(encoding="utf-8"))
    if not isinstance(data, list):
        raise ValueError(f"{path}: expected a top-level YAML list of cases")
    cases: list[Case] = []
    for i, entry in enumerate(data):
        if not isinstance(entry, dict):
            raise ValueError(f"{path}[{i}]: case must be a mapping")
        name = entry.get("name")
        raw = entry.get("raw")
        if not isinstance(name, str) or not name:
            raise ValueError(f"{path}[{i}]: each case needs a string `name`")
        if not isinstance(raw, str) or not raw:
            raise ValueError(f"{path}[{i}] ({name}): each case needs a string `raw`")
        cases.append(
            Case(
                name=name,
                raw=raw,
                must_contain=list(entry.get("must_contain") or []),
                must_not_contain=list(entry.get("must_not_contain") or []),
                note=entry.get("note"),
            )
        )
    return cases


def _call_sync(
    *,
    base_url: str,
    api_key: str,
    model: str,
    prompt: str,
    raw: str,
    timeout_s: float,
) -> tuple[str, Optional[dict]]:
    url = base_url.rstrip("/") + "/chat/completions"
    body = {
        "model": model,
        "messages": [
            {"role": "system", "content": prompt},
            {"role": "user", "content": raw},
        ],
        "stream": False,
        "temperature": 0.2,
        "max_tokens": 4096,
        # DashScope Qwen3-specific knob — silently ignored by other
        # OpenAI-compatible providers. Match core's behavior.
        "enable_thinking": False,
    }
    data = json.dumps(body).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=data,
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {api_key}",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout_s) as resp:
            payload = json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", errors="replace")[:500]
        raise RuntimeError(f"HTTP {exc.code}: {detail}") from None

    choices = payload.get("choices") or []
    if not choices:
        raise RuntimeError(f"response has no choices: {payload}")
    content = choices[0].get("message", {}).get("content", "")
    if not isinstance(content, str) or not content.strip():
        raise RuntimeError(f"empty LLM content: {payload}")
    return content.strip(), payload.get("usage")


async def _run_one(
    case: Case,
    *,
    base_url: str,
    api_key: str,
    model: str,
    prompt: str,
    timeout_s: float,
) -> Result:
    t0 = time.perf_counter()
    try:
        cleaned, usage = await asyncio.to_thread(
            _call_sync,
            base_url=base_url,
            api_key=api_key,
            model=model,
            prompt=prompt,
            raw=case.raw,
            timeout_s=timeout_s,
        )
        return Result(
            case=case,
            cleaned=cleaned,
            latency_ms=(time.perf_counter() - t0) * 1000,
            usage=usage,
            error=None,
        )
    except (urllib.error.URLError, RuntimeError) as exc:
        return Result(
            case=case,
            cleaned=None,
            latency_ms=(time.perf_counter() - t0) * 1000,
            usage=None,
            error=str(exc),
        )


def _check_assertions(result: Result) -> tuple[bool, list[str]]:
    if result.cleaned is None:
        return False, [f"call failed: {result.error}"]
    failures: list[str] = []
    for needle in result.case.must_contain:
        if needle not in result.cleaned:
            failures.append(f'must_contain "{needle}"')
    for needle in result.case.must_not_contain:
        if needle in result.cleaned:
            failures.append(f'must_not_contain "{needle}"')
    return not failures, failures


def _print_result(result: Result) -> bool:
    passed, failures = _check_assertions(result)
    tag = "PASS" if passed else "FAIL"
    print(f"[{tag}] {result.case.name}")
    if result.case.note:
        print(f"  note:   {result.case.note}")
    print(f"  raw:    {result.case.raw}")
    if result.cleaned is not None:
        print(f"  clean:  {result.cleaned}")
    else:
        print(f"  error:  {result.error}")
    bits = [f"{result.latency_ms:.0f}ms"]
    if result.usage:
        pt = result.usage.get("prompt_tokens")
        ct = result.usage.get("completion_tokens")
        if pt is not None or ct is not None:
            bits.append(f"{pt if pt is not None else '?'}p+{ct if ct is not None else '?'}c")
    print(f"  stats:  {' · '.join(bits)}")
    for f in failures:
        print(f"  └─ FAIL: {f}")
    print()
    return passed


async def promptlab(
    *,
    prompt_path: Path,
    cases_path: Path,
    base_url: str,
    model: str,
    timeout_s: float,
    only: Optional[list[str]],
) -> int:
    api_key = os.environ.get("AIRTALK_LLM_API_KEY")
    if not api_key:
        print("error: AIRTALK_LLM_API_KEY must be set in env", file=sys.stderr)
        return 2

    prompt = prompt_path.read_text(encoding="utf-8")
    cases = load_cases(cases_path)
    if only:
        wanted = set(only)
        cases = [c for c in cases if c.name in wanted]
        if not cases:
            print(f"error: no cases matched {only!r}", file=sys.stderr)
            return 2

    print(f"prompt: {prompt_path}  ({len(prompt)} chars)")
    print(f"cases:  {cases_path}  ({len(cases)} selected)")
    print(f"model:  {model}")
    print()

    results = await asyncio.gather(
        *(
            _run_one(
                c,
                base_url=base_url,
                api_key=api_key,
                model=model,
                prompt=prompt,
                timeout_s=timeout_s,
            )
            for c in cases
        )
    )

    passed = 0
    for r in results:
        if _print_result(r):
            passed += 1

    total_ms = sum(r.latency_ms for r in results)
    print(f"Summary: {passed}/{len(results)} passed  ({total_ms:.0f}ms total LLM time)")
    return 0 if passed == len(results) else 1
