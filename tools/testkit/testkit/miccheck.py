"""Interactive streaming mic → core → result loop.

Spawns one long-lived core on startup. Each round:

1. Sends `begin`.
2. Opens a sounddevice InputStream that buffers captured audio.
3. Every ~100ms, drains the buffer and sends a `chunk` frame while
   the user is still speaking — so by the time they press Enter,
   core's VAD has already split segments and ASR is partly done.
4. On Enter: flush remaining audio, send `end`, wait for terminal
   response. The gap between "end sent" and "result received" is
   the perceived-latency metric (`wait`).

Ctrl+C quits the loop; the session is closed gracefully.
"""

from __future__ import annotations

import asyncio
import base64
import sys
import threading
import time
import wave
from pathlib import Path
from typing import Optional

import numpy as np
import sounddevice as sd

from .core_session import CoreSession


SAMPLE_RATE = 16_000
CHUNK_INTERVAL_S = 0.1  # flush mic buffer into core every 100 ms


def _write_pcm16_wav(samples_f32: np.ndarray, path: Path) -> None:
    pcm16 = np.clip(samples_f32 * 32767.0, -32768, 32767).astype(np.int16)
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SAMPLE_RATE)
        w.writeframes(pcm16.tobytes())


async def miccheck(
    *,
    core_bin: Path,
    vad: bool,
    no_llm: bool,
    llm_base_url: Optional[str],
    llm_model: Optional[str],
    context: Optional[str],
    auto_lang: bool,
    save_to: Optional[Path],
) -> int:
    print("miccheck — Ctrl+C at any time to quit.", file=sys.stderr)
    if context:
        preview = context if len(context) <= 80 else context[:77] + "..."
        print(f"context: {preview}", file=sys.stderr)
    if auto_lang:
        print("language: auto (LID enabled)", file=sys.stderr)

    session = await CoreSession.spawn(
        core_bin=core_bin,
        no_llm=no_llm,
        llm_base_url=llm_base_url,
        llm_model=llm_model,
        log_level="info",
    )

    round_no = 0
    try:
        while True:
            round_no += 1
            if round_no > 1:
                try:
                    await asyncio.to_thread(
                        input,
                        "\nPress Enter to record again (Ctrl+C to quit): ",
                    )
                except (EOFError, KeyboardInterrupt, asyncio.CancelledError):
                    # User wants to quit between rounds.
                    break
            try:
                await _one_round(
                    session,
                    vad=vad,
                    round_no=round_no,
                    context=context,
                    auto_lang=auto_lang,
                    save_to=save_to,
                )
            except (KeyboardInterrupt, asyncio.CancelledError):
                # User interrupted mid-round — stop the loop.
                break
            except Exception as exc:
                # Don't kill the whole loop for one bad round — the
                # session itself might still be fine.
                print(f"[round {round_no} error] {exc}", file=sys.stderr)
    finally:
        # Clean shutdown of core (close stdin → graceful drain → exit).
        # Shield so a stacked-up CancelledError can't abort the close
        # before we've actually signalled shutdown.
        try:
            await asyncio.shield(session.close())
        except asyncio.CancelledError:
            pass
    print("\n[quit]")
    return 0


async def _one_round(
    session: CoreSession,
    *,
    vad: bool,
    round_no: int,
    context: Optional[str],
    auto_lang: bool,
    save_to: Optional[Path],
) -> None:
    sid = session.next_id()

    # Two lists share the audio callback:
    #  - `pending` is drained on each pump tick and flushed as a chunk
    #  - `all_frames` accumulates the whole recording for optional --save
    pending: list[np.ndarray] = []
    all_frames: list[np.ndarray] = []
    lock = threading.Lock()

    def callback(indata, _frames_count, _time_info, status) -> None:
        if status:
            print(f"[audio status] {status}", file=sys.stderr)
        arr = indata.copy()
        with lock:
            pending.append(arr)
            all_frames.append(arr)

    # Begin BEFORE the stream opens, so core is ready to receive chunks
    # as soon as audio starts arriving.
    begin_frame: dict = {"type": "begin", "id": sid, "vad": vad}
    if context:
        begin_frame["context"] = context
    if auto_lang:
        begin_frame["language"] = "auto"
    await session.send(begin_frame)

    stop_event = asyncio.Event()

    async def pump() -> None:
        """Flush the mic buffer to core every CHUNK_INTERVAL_S."""
        while not stop_event.is_set():
            await asyncio.sleep(CHUNK_INTERVAL_S)
            await _flush_pending(session, sid, pending, lock)

    print(
        f"🎙️  Recording round {round_no}... press Enter to stop.",
        flush=True,
    )
    pump_task = asyncio.create_task(pump())
    t_start = time.time()

    try:
        with sd.InputStream(
            samplerate=SAMPLE_RATE,
            channels=1,
            dtype="float32",
            blocksize=1600,  # 100ms at 16 kHz
            callback=callback,
        ):
            await asyncio.to_thread(input)
    except (KeyboardInterrupt, EOFError, asyncio.CancelledError):
        # Stop the pump + the mic stream (InputStream closes via `with`
        # already), then propagate so the loop exits. We intentionally
        # do NOT send a Cancel frame here: stdin close in the outer
        # finally triggers graceful shutdown, core drains whatever's
        # in flight, and we don't care about the orphan Result.
        stop_event.set()
        pump_task.cancel()
        raise

    t_stopped = time.time()
    stop_event.set()
    try:
        await pump_task
    except asyncio.CancelledError:
        pass

    # Last tick may have fired <100ms before Enter, so flush once more.
    await _flush_pending(session, sid, pending, lock)

    t_end_sent = time.time()
    await session.send({"type": "end", "id": sid})
    response = await session.recv_terminal(sid)
    t_received = time.time()

    rec_s = t_stopped - t_start
    wait_ms = (t_received - t_end_sent) * 1000.0
    total_ms = (t_received - t_start) * 1000.0

    if save_to is not None and all_frames:
        audio = np.concatenate(all_frames).flatten()
        save_path = save_to.with_stem(f"{save_to.stem}_{round_no:03d}")
        save_path.parent.mkdir(parents=True, exist_ok=True)
        _write_pcm16_wav(audio, save_path)
        print(f"saved recording to {save_path}")

    _print_result(response, rec_s=rec_s, wait_ms=wait_ms, total_ms=total_ms)


async def _flush_pending(
    session: CoreSession,
    sid: int,
    pending: list[np.ndarray],
    lock: threading.Lock,
) -> None:
    """Drain `pending` (thread-safe), base64, send as one chunk frame."""
    with lock:
        if not pending:
            return
        combined = np.concatenate(pending).flatten()
        pending.clear()
    pcm16 = np.clip(combined * 32767.0, -32768, 32767).astype(np.int16).tobytes()
    b64 = base64.b64encode(pcm16).decode("ascii")
    await session.send({"type": "chunk", "id": sid, "pcm": b64})


def _print_result(
    resp: dict,
    *,
    rec_s: float,
    wait_ms: float,
    total_ms: float,
) -> None:
    sep = "─" * 60
    print(sep)
    if resp.get("type") == "error":
        print(f"error:    {resp.get('message')!r}")
    else:
        text = resp.get("text", "")
        raw = resp.get("raw")
        lang = resp.get("language")
        print(f"text:     {text}")
        if raw is not None:
            if raw != text:
                print(f"raw:      {raw}")
            else:
                # Core always returns `raw`; suppressing when identical
                # used to be ambiguous — could mean LLM skipped, or LLM
                # ran but produced no change. Print a marker so the
                # distinction from "no raw field" is visible.
                print("raw:      (same as text)")
        if lang:
            print(f"language: {lang}")
        _print_stats(resp.get("stats"))
    print(
        f"rec: {rec_s:5.2f}s   "
        f"wait: {wait_ms:5.0f}ms (end→result)   "
        f"total: {total_ms:5.0f}ms"
    )
    print(sep)


def _print_stats(stats: Optional[dict]) -> None:
    """Compact stats block. Three lines max; sections skipped when
    their data isn't present (VAD off, LLM disabled, provider omitted
    `usage`)."""
    if not stats:
        return

    # ── Audio / VAD line ──
    audio_bits: list[str] = []
    segs = stats.get("vad_segments")
    if isinstance(segs, int):
        audio_bits.append(f"{segs} segs")
    recv_ms = stats.get("pcm_received_ms")
    sent_ms = stats.get("pcm_sent_to_asr_ms")
    if isinstance(recv_ms, int) and isinstance(sent_ms, int):
        if sent_ms != recv_ms:
            audio_bits.append(f"{sent_ms}ms spoken / {recv_ms}ms total")
        else:
            audio_bits.append(f"{recv_ms}ms audio")
    if audio_bits:
        print(f"audio:    {' · '.join(audio_bits)}")

    # ── ASR line ──
    asr_bits: list[str] = []
    calls = stats.get("asr_calls")
    if isinstance(calls, int):
        asr_bits.append(f"{calls} call{'s' if calls != 1 else ''}")
    upload = stats.get("asr_upload_bytes")
    if isinstance(upload, int):
        asr_bits.append(_fmt_bytes(upload) + " up")
    asr_latency = stats.get("asr_latency_ms")
    if isinstance(asr_latency, int):
        asr_bits.append(f"{asr_latency}ms")
    asr_usage = stats.get("asr_usage") or {}
    usage_parts: list[str] = []
    # DashScope bills Qwen3-ASR by audio seconds — lead with that.
    audio_s = asr_usage.get("audio_seconds")
    if isinstance(audio_s, (int, float)):
        usage_parts.append(f"{audio_s:.0f}s billed")
    # Token info, if present (OpenAI-compat endpoint surfaces totals).
    tot = asr_usage.get("total_tokens")
    out_tok = asr_usage.get("output_tokens")
    if isinstance(tot, int):
        usage_parts.append(f"{tot} tok")
    elif isinstance(out_tok, int):
        usage_parts.append(f"{out_tok} out-tok")
    if usage_parts:
        asr_bits.append("usage " + " / ".join(usage_parts))
    if asr_bits:
        print(f"asr:      {' · '.join(asr_bits)}")

    # ── LLM line ──
    llm_bits: list[str] = []
    llm_latency = stats.get("llm_latency_ms")
    if isinstance(llm_latency, int):
        llm_bits.append(f"{llm_latency}ms")
    llm_usage = stats.get("llm_usage") or {}
    pt = llm_usage.get("prompt_tokens")
    ct = llm_usage.get("completion_tokens")
    if isinstance(pt, int) or isinstance(ct, int):
        pt_s = pt if isinstance(pt, int) else "?"
        ct_s = ct if isinstance(ct, int) else "?"
        llm_bits.append(f"usage {pt_s}p+{ct_s}c")
    if llm_bits:
        print(f"llm:      {' · '.join(llm_bits)}")


def _fmt_bytes(n: int) -> str:
    if n < 1024:
        return f"{n}B"
    if n < 1024 * 1024:
        return f"{n / 1024:.1f}KB"
    return f"{n / (1024 * 1024):.2f}MB"
