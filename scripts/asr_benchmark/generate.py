#!/usr/bin/env python3
"""用 qwen3-tts-flash 生成测试语料音频（缓存到 audio/，可重复执行）。

用法:
    nix develop --command python scripts/asr_benchmark/generate.py
    nix develop --command python scripts/asr_benchmark/generate.py --split test
    nix develop --command python scripts/asr_benchmark/generate.py --api-key-env BAILIAN_API_KEY --hard

--hard: 强制重新生成已存在的音频（默认跳过缓存）。

Corpus splits (corpus/<split>.jsonl, one `{"id", "reference", "voice"}`
object per line): `dev` is the tuning set (24 entries, Cherry/Ethan
voices), `test` the frozen regression set (12 entries, Serena voice, so
no voice leaks between the two); `--split all` globs every *.jsonl in
corpus/, which also picks up real.jsonl. `reference` is both the TTS
input and the ground truth the Rust gate (`voiceless benchmark asr`,
rust/src/benchmark.rs) scores against; ids must stay disjoint across
dev and test (rust/tests/benchmark_contract.rs checks this).

Audio: each entry is synthesized once with DashScope's qwen3-tts-flash
(16 kHz WAV, the rate the realtime backends expect) into audio/<id>.wav.
Existing files are skipped unless --hard, so re-running only fills gaps;
a failed entry is reported and the run continues, exit status 1 if any
failed. audio/ is gitignored (*.wav): every checkout regenerates it from
the same references, which is what makes the corpus reproducible.
"""

from __future__ import annotations

import argparse
import json
import os
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent
CORPUS_DIR = ROOT / "corpus"
AUDIO_DIR = ROOT / "audio"
TTS_URL = "https://dashscope.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation"
TTS_MODEL = "qwen3-tts-flash"
SAMPLE_RATE = 16000


def _api_key(env_name: str) -> str:
    """Read the DashScope key from `env_name`; exit with a message if unset."""
    value = os.environ.get(env_name, "")
    if not value:
        raise SystemExit(f"缺少 API 密钥：请设置环境变量 {env_name}")
    return value


def synthesize(text: str, voice: str, api_key: str, timeout: float = 120.0) -> bytes:
    """Return the WAV bytes for `text` spoken by `voice`.

    Two HTTP round trips: the generation call answers with a temporary
    audio URL (`output.audio.url`), which is then downloaded. Any HTTP
    error, timeout or missing key in the response propagates to the
    caller.
    """
    payload = {
        "model": TTS_MODEL,
        "input": {
            "text": text,
            "voice": voice,
            "format": "wav",
            "sample_rate": SAMPLE_RATE,
        },
    }
    request = urllib.request.Request(
        TTS_URL,
        data=json.dumps(payload).encode("utf-8"),
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
        },
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        body = json.loads(response.read().decode("utf-8"))
    audio_url = body["output"]["audio"]["url"]
    with urllib.request.urlopen(audio_url, timeout=timeout) as response:
        return response.read()


def load_entries(split: str) -> list[dict]:
    """Parse corpus/<split>.jsonl (or every *.jsonl for `all`), blank lines skipped."""
    paths = (
        [CORPUS_DIR / f"{split}.jsonl"] if split != "all" else sorted(CORPUS_DIR.glob("*.jsonl"))
    )
    entries: list[dict] = []
    for path in paths:
        for line in path.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if line:
                entries.append(json.loads(line))
    return entries


def main() -> int:
    """Synthesize missing (or, with --hard, all) WAVs; 0 if nothing failed.

    Each file is written to <id>.tmp and renamed into place, so an
    interrupted download never leaves a truncated WAV that the next run
    would treat as cached. Requests are spaced 0.2 s apart.
    """
    parser = argparse.ArgumentParser(description="生成 ASR 基准测试语料音频")
    parser.add_argument("--split", choices=("dev", "test", "all"), default="all")
    parser.add_argument("--api-key-env", default="BAILIAN_API_KEY")
    parser.add_argument("--hard", action="store_true", help="重新生成已缓存的音频")
    args = parser.parse_args()

    api_key = _api_key(args.api_key_env)
    AUDIO_DIR.mkdir(parents=True, exist_ok=True)
    entries = load_entries(args.split)
    ok = skipped = failed = 0
    for entry in entries:
        target = AUDIO_DIR / f"{entry['id']}.wav"
        if target.is_file() and not args.hard:
            skipped += 1
            continue
        try:
            data = synthesize(entry["reference"], entry["voice"], api_key)
            tmp = target.with_suffix(".tmp")
            tmp.write_bytes(data)
            tmp.replace(target)
            ok += 1
            print(f"生成 {target.name}（{entry['voice']}，{len(data)} 字节）")
        except Exception as exc:  # noqa: BLE001 - 基准工具需要逐个样本继续
            failed += 1
            print(f"失败 {entry['id']}: {exc}")
        time.sleep(0.2)
    print(f"完成：新增 {ok}，跳过 {skipped}，失败 {failed}")
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
