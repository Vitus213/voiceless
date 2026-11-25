#!/usr/bin/env python3
"""Replay the local-offline CLI at microphone pace and print one JSON row per trial."""

import argparse
import asyncio
import json
import math
import os
from pathlib import Path
import random
import shlex
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time

from normalize import cer

RATE = 16_000
CHUNK_BYTES = 1_024
BENCHMARK = Path(__file__).resolve().parent
CONDITIONS = ("clean", "far-0db", "far-5db", "noise-only")


def replay_capture():
    audio = Path(os.environ["VOICE_REPLAY_PCM"]).read_bytes()
    signal.signal(signal.SIGINT, signal.SIG_DFL)
    start = time.monotonic()
    for offset in range(0, len(audio), CHUNK_BYTES):
        end = min(offset + CHUNK_BYTES, len(audio))
        deadline = start + end / (2 * RATE)
        time.sleep(max(0, deadline - time.monotonic()))
        sys.stdout.buffer.write(audio[offset:end])
        sys.stdout.buffer.flush()
    # Keep the microphone open until the CLI stops it. EOF means device loss.
    os.write(int(os.environ["VOICE_REPLAY_TIMING_FD"]), struct.pack("!dd", start, time.monotonic()))
    signal.pause()


def audio_variants(pcm):
    yield "clean", pcm
    source = [sample for (sample,) in struct.iter_unpack("<h", pcm)]
    reflections = [(384, 0.28), (752, -0.17), (1264, 0.13), (2080, 0.08)]
    reverberant = []
    low_pass = 0.0
    attenuation = 10 ** (-18 / 20)
    for index, sample in enumerate(source):
        reflected = sum(weight * source[index - lag] for lag, weight in reflections if index >= lag)
        low_pass = 0.4 * low_pass + 0.6 * (sample + reflected)
        reverberant.append(low_pass * attenuation)

    rng = random.Random(72355)
    noise = []
    filtered = 0.0
    for index in range(len(source)):
        filtered = 0.8 * filtered + 0.2 * rng.gauss(0, 1)
        noise.append(filtered + 0.12 * math.sin(2 * math.pi * 145 * index / RATE))
    scale = math.sqrt(sum(s * s for s in reverberant) / sum(n * n for n in noise))
    yield "noise-only", pcm16(scale * value for value in noise)
    for snr in (0, 5):
        noise_gain = scale * 10 ** (-snr / 20)
        yield f"far-{snr}db", pcm16(s + noise_gain * n for s, n in zip(reverberant, noise))


def pcm16(samples):
    return b"".join(struct.pack("<h", min(32767, max(-32768, round(sample)))) for sample in samples)


async def measure(binary, config, scratch, pcm, reference, timing, recorder_timing):
    input_path = scratch / "input.pcm"
    input_path.write_bytes(pcm)
    env = dict(
        os.environ,
        PATH=f"{scratch}{os.pathsep}{os.environ['PATH']}",
        XDG_CONFIG_HOME=str(scratch / "config"),
        VOICE_REPLAY_PCM=str(input_path),
        VOICE_REPLAY_TIMING_FD=str(recorder_timing.fileno()),
    )
    launched = time.monotonic()
    process = await asyncio.create_subprocess_exec(
        binary, "--config", str(config), "stream", "--json", "--no-inject",
        stdout=asyncio.subprocess.PIPE, env=env, pass_fds=(recorder_timing.fileno(),),
    )
    updates = []
    final_time = None
    transcript = ""
    audio_seconds = len(pcm) / (2 * RATE)
    stamps = None
    loop = asyncio.get_running_loop()

    def stop_after_audio():
        nonlocal stamps
        first, last = struct.unpack("!dd", timing.recv(16))
        stamps = first, last, time.monotonic()
        if process.returncode is None:
            process.send_signal(signal.SIGINT)

    loop.add_reader(timing, stop_after_audio)
    try:
        async with asyncio.timeout(audio_seconds + 90):
            async for line in process.stdout:
                received = time.monotonic()
                event = json.loads(line)
                if event["type"] == "error":
                    raise RuntimeError(event["message"])
                if (snapshot := event.get("transcript")) and snapshot["authoritative_text"]:
                    if snapshot["is_final"]:
                        final_time = received
                        transcript = snapshot["authoritative_text"]
                    else:
                        updates.append(received)
            if await process.wait() != 0:
                raise RuntimeError(f"voiceless exited with status {process.returncode}")
    finally:
        loop.remove_reader(timing)
        if process.returncode is None:
            process.kill()
            await process.wait()

    if stamps is None:
        raise RuntimeError("replay ended before the recorder delivered the complete sample")
    start, end, stopped_at = stamps
    live = [timestamp for timestamp in updates if timestamp < end]
    observations = [start, *live, end]
    return {
        "audio_seconds": audio_seconds,
        "capture_start": start - launched,
        "first_text": live[0] - start if live else None,
        "max_live_gap": max(b - a for a, b in zip(observations, observations[1:])),
        "live_updates": len(live),
        "stop_final": final_time - stopped_at if final_time is not None else None,
        "cer": cer(reference, transcript) if reference else None,
        "transcript": transcript,
    }


async def run(args):
    corpus = BENCHMARK / "corpus" / f"{args.split}.jsonl"
    entries = [json.loads(line) for line in corpus.read_text().splitlines() if line.strip()]
    available = {row["id"] for row in entries}
    if args.sample and (unknown := set(args.sample) - available):
        raise SystemExit(f"Unknown samples in {corpus}: {', '.join(sorted(unknown))}")
    conditions = set(args.condition or CONDITIONS)
    timing, recorder_timing = socket.socketpair(type=socket.SOCK_DGRAM)
    with timing, recorder_timing, tempfile.TemporaryDirectory(prefix="voiceless-replay-") as directory:
        scratch = Path(directory)
        recorder = scratch / "pw-record"
        capture = shlex.join([sys.executable, str(Path(__file__).resolve()), "--capture"])
        recorder.write_text(
            '#!/bin/sh\nif [ "$1" = --help ]; then echo "--properties --raw"; exit 0; fi\n'
            f"exec {capture}\n"
        )
        recorder.chmod(0o700)
        graph = scratch / "pw-dump"
        graph.write_text("#!/bin/sh\nprintf '[]\\n'\n")
        graph.chmod(0o700)
        config = scratch / "session.toml"
        settings = (
            '[asr]\nstreaming_backend = "local-offline"\n[local]\n'
            f"model = {json.dumps(args.model)}\nprovider = {json.dumps(args.provider)}\n"
        )
        if args.denoise:
            settings += "denoise = true\n"
        if args.fast_preview:
            settings += "fast_preview = true\n"
        config.write_text(settings + (
            "[history]\nenabled = false\nsave_audio = false\n"
            "[audio]\npause_media = false\ncues = false\n"
            "[console]\nenabled = false\n"
        ))
        for row in entries:
            sample = row["id"]
            if args.sample and sample not in args.sample:
                continue
            path = BENCHMARK / "audio" / f"{sample}.wav"
            pcm = subprocess.check_output([
                "ffmpeg", "-v", "error", "-i", str(path), "-ar", str(RATE), "-ac", "1", "-f", "s16le", "-",
            ])
            if not pcm:
                raise RuntimeError(f"Empty audio: {path}")
            for condition, audio in audio_variants(pcm):
                if condition in conditions:
                    reference = "" if condition == "noise-only" else row["reference"]
                    result = await measure(args.binary, config, scratch, audio, reference, timing, recorder_timing)
                    print(json.dumps(dict(sample=sample, condition=condition, **result), ensure_ascii=False), flush=True)


def main():
    if sys.argv[1:] == ["--capture"]:
        replay_capture()
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="voiceless", help="path or command for the build under test")
    parser.add_argument("--model", default="sense-voice-int8")
    parser.add_argument("--provider", choices=("cpu", "cuda", "vulkan", "auto"), default="cpu")
    parser.add_argument("--denoise", action="store_true")
    parser.add_argument("--fast-preview", action="store_true", help="use SenseVoice previews with Qwen ONNX or GGUF")
    parser.add_argument("--split", choices=("test", "dev"), default="test")
    parser.add_argument("--sample", action="append", help="run a specific corpus ID (repeatable)")
    parser.add_argument("--condition", choices=CONDITIONS, action="append")
    asyncio.run(run(parser.parse_args()))


if __name__ == "__main__":
    main()
