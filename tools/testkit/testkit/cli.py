"""CLI entry point: `python -m testkit run|run-all ...`"""

from __future__ import annotations

import argparse
import asyncio
import sys
from pathlib import Path
from typing import Optional

import yaml

from .compose import build_audio
from .driver import run_core
from .metrics import CaseReport, evaluate


_PROJECT_ROOT = Path(__file__).resolve().parents[3]
DEFAULT_CORE_BIN = str(_PROJECT_ROOT / "target" / "debug" / "airtalk-core.exe")
DEFAULT_OUT_DIR = str(_PROJECT_ROOT / "tools" / "testkit" / "outputs")


async def run_case(case_path: Path, out_dir: Path, core_bin: Path) -> CaseReport:
    spec = yaml.safe_load(case_path.read_text(encoding="utf-8"))
    wav_path = out_dir / f"{spec['name']}.wav"

    await build_audio(spec, wav_path)

    core_section = spec.get("core", {})
    core_run = await run_core(
        wav_path,
        core_bin=core_bin,
        vad=bool(core_section.get("vad", True)),
        no_llm=bool(core_section.get("no_llm", True)),
        context=core_section.get("context"),
        language=core_section.get("language"),
    )

    return evaluate(spec, core_run)


def _print_report(r: CaseReport) -> None:
    tag = "PASS" if r.overall_pass else "FAIL"
    bits = []
    if r.vad_segments is not None:
        bits.append(f"segments={r.vad_segments}")
    if r.cer is not None:
        bits.append(f"CER={r.cer:.3f}")
    if r.language:
        bits.append(f"lang={r.language}")
    bits.append(f"took={r.elapsed_s:.1f}s")
    summary = "  ".join(bits)
    print(f"[{tag}] {r.name:<30} {summary}")

    if r.failure_reason:
        print(f"         └─ {r.failure_reason}")
    for a in r.assertions:
        if not a.passed:
            msg = a.name
            if a.detail:
                msg += f" ({a.detail})"
            print(f"         └─ FAIL: {msg}")
    if not r.overall_pass and r.raw_text:
        print(f"         └─ raw: {r.raw_text!r}")


def main() -> None:
    parser = argparse.ArgumentParser(prog="python -m testkit")
    sub = parser.add_subparsers(dest="cmd", required=True)

    for name in ("run", "run-all"):
        sp = sub.add_parser(name)
        sp.add_argument("target", help="case YAML (run) or cases directory (run-all)")
        sp.add_argument("--core-bin", default=DEFAULT_CORE_BIN)
        sp.add_argument("--out-dir", default=DEFAULT_OUT_DIR)

    p_mic = sub.add_parser(
        "miccheck",
        help="Record from mic, send to core, print the recognized text",
    )
    p_mic.add_argument("--core-bin", default=DEFAULT_CORE_BIN)
    p_mic.add_argument(
        "--no-vad", action="store_true", help="send with vad=false"
    )
    p_mic.add_argument(
        "--no-llm",
        action="store_true",
        help="skip LLM cleanup (default: clean with qwen-flash)",
    )
    p_mic.add_argument(
        "--llm-base-url",
        default="https://dashscope.aliyuncs.com/compatible-mode/v1",
    )
    p_mic.add_argument("--llm-model", default="qwen-flash")
    p_mic.add_argument(
        "--context",
        help="ASR context / glossary sent with each begin frame "
        "(domain hints, hotwords, anchor terms)",
    )
    p_mic.add_argument(
        "--auto-lang",
        action="store_true",
        help="enable language identification: send language=\"auto\" "
        "instead of inheriting core's default (usually zh)",
    )
    p_mic.add_argument(
        "--context-file",
        help="read ASR context from a file (wins over --context if both given)",
    )
    p_mic.add_argument(
        "--save", help="save the recorded WAV to this path for replay"
    )

    args = parser.parse_args()

    core_bin = Path(args.core_bin)
    if not core_bin.is_file():
        print(
            f"error: core binary not found at {core_bin}\n"
            f"hint: run `cargo build -p airtalk-core` from the project root, "
            f"or pass --core-bin <path>",
            file=sys.stderr,
        )
        sys.exit(2)

    if args.cmd == "miccheck":
        # Lazy-import so the testkit CLI still works on machines without
        # sounddevice/PortAudio (which aren't needed for run/run-all).
        from .miccheck import miccheck

        save_to = Path(args.save) if args.save else None

        context: Optional[str] = None
        if args.context_file:
            context = Path(args.context_file).read_text(encoding="utf-8").strip()
        elif args.context:
            context = args.context

        try:
            exit_code = asyncio.run(
                miccheck(
                    core_bin=core_bin,
                    vad=not args.no_vad,
                    no_llm=args.no_llm,
                    llm_base_url=None if args.no_llm else args.llm_base_url,
                    llm_model=None if args.no_llm else args.llm_model,
                    context=context,
                    auto_lang=args.auto_lang,
                    save_to=save_to,
                )
            )
        except KeyboardInterrupt:
            # Python 3.14 re-raises KeyboardInterrupt after the coro's
            # own CancelledError handling has already run — swallow it
            # so the user just sees a clean exit code.
            exit_code = 0
        sys.exit(exit_code)

    # From here on: run / run-all both need an out_dir.
    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    if args.cmd == "run":
        case = Path(args.target)
        report = asyncio.run(run_case(case, out_dir, core_bin))
        _print_report(report)
        sys.exit(0 if report.overall_pass else 1)

    # run-all
    cases_dir = Path(args.target)
    case_paths = sorted(cases_dir.glob("*.yaml"))
    if not case_paths:
        print(f"no .yaml cases found in {cases_dir}")
        sys.exit(2)

    reports = []
    for cp in case_paths:
        report = asyncio.run(run_case(cp, out_dir, core_bin))
        _print_report(report)
        reports.append(report)

    passed = sum(1 for r in reports if r.overall_pass)
    print(f"\nSummary: {passed}/{len(reports)} passed")
    sys.exit(0 if passed == len(reports) else 1)
