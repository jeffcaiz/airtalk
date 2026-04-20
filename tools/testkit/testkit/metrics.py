"""Assertion + metric computation for a single case run."""

from __future__ import annotations

import re
import unicodedata
from dataclasses import dataclass, field
from typing import Optional

from jiwer import cer as _jiwer_cer

from .driver import CoreRun


@dataclass
class Assertion:
    name: str
    passed: bool
    detail: str = ""


@dataclass
class CaseReport:
    name: str
    overall_pass: bool
    raw_text: str
    final_text: str
    language: Optional[str]
    vad_segments: Optional[int]
    cer: Optional[float]
    elapsed_s: float
    assertions: list[Assertion] = field(default_factory=list)
    failure_reason: Optional[str] = None


# Light normalization: strip common punctuation + collapse whitespace.
# Runs on both reference (TTS script) and hypothesis (ASR output) so
# CER measures content difference, not punctuation style.
_PUNCT_RE = re.compile(
    r"[\s"
    r"，。！？、；：…—\-·「」『』（）《》〈〉\"'“”‘’"
    r",.!?;:\[\](){}/<>\"'"
    r"]+",
    flags=re.UNICODE,
)


def _normalize(text: str) -> str:
    text = unicodedata.normalize("NFKC", text)
    text = text.lower()
    return _PUNCT_RE.sub("", text)


def evaluate(spec: dict, run: CoreRun) -> CaseReport:
    name = spec["name"]
    expect = spec.get("expect", {})

    report = CaseReport(
        name=name,
        overall_pass=False,
        raw_text="",
        final_text="",
        language=None,
        vad_segments=run.vad_segments,
        cer=None,
        elapsed_s=run.elapsed_s,
    )

    if run.response is None:
        report.failure_reason = f"core did not emit a terminal response (exit={run.exit_code})"
        return report

    if run.response.get("type") == "error":
        report.failure_reason = f"core returned error: {run.response.get('message')!r}"
        return report

    # It's a result.
    report.final_text = run.response.get("text", "")
    report.raw_text = run.response.get("raw") or report.final_text
    report.language = run.response.get("language")

    # Compute CER against the concatenation of `say` segments.
    reference = " ".join(
        seg["say"] for seg in spec.get("segments", []) if "say" in seg
    )
    if reference:
        ref_norm = _normalize(reference)
        hyp_norm = _normalize(report.raw_text)
        if ref_norm:
            report.cer = float(_jiwer_cer(ref_norm, hyp_norm))

    # ─── Assertions ───────────────────────────────────────────────────
    if "segments_emitted" in expect:
        expected_n = int(expect["segments_emitted"])
        actual_n = run.vad_segments
        report.assertions.append(
            Assertion(
                name=f"segments_emitted == {expected_n}",
                passed=(actual_n is not None and actual_n == expected_n),
                detail=f"actual={actual_n}",
            )
        )

    if "raw_cer_max" in expect:
        limit = float(expect["raw_cer_max"])
        passed = report.cer is not None and report.cer <= limit
        report.assertions.append(
            Assertion(
                name=f"CER <= {limit:.3f}",
                passed=passed,
                detail=f"actual={report.cer:.3f}" if report.cer is not None else "no CER",
            )
        )

    for keyword in expect.get("raw_must_contain", []):
        passed = keyword in report.raw_text
        report.assertions.append(
            Assertion(
                name=f"raw contains {keyword!r}",
                passed=passed,
            )
        )

    if "language" in expect:
        want = expect["language"]
        passed = report.language == want
        report.assertions.append(
            Assertion(
                name=f"language == {want!r}",
                passed=passed,
                detail=f"actual={report.language!r}",
            )
        )

    report.overall_pass = all(a.passed for a in report.assertions) and bool(
        report.assertions
    )
    return report
