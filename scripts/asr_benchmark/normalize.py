#!/usr/bin/env python3
"""标点无关/相关中文 CER 与英文 WER 计算（基准测试度量）。

CER: 编辑距离 / 参考长度（按字符）。
- content 模式：忽略所有标点与空白、NFKC、转小写 —— 衡量识别内容准确度。
- format 模式：保留标点但忽略空白 —— 衡量结构（含标点）准确度。
WER: 参考文本中 ASCII 单词（含数字）按词编辑距离。

Reference definitions of the benchmark metrics. Nothing in the repository
imports this module any more (the Python benchmark runner it served was
replaced by `voiceless benchmark asr`); it is kept because
rust/src/benchmark.rs reimplements `normalize_content` and the content
`cer` and must stay equivalent to it, so the Rust reports remain
comparable with the frozen baselines in README.md.

Why normalize: ASR output differs from the reference in punctuation,
full-width vs ASCII forms, letter case and spacing, none of which is a
recognition error for voice input (the `[text]` cleanup rewrites them
anyway). Content mode removes all of that before the edit distance so the
CER measures only what was heard. Format mode keeps punctuation to judge
the structure separately. WER looks only at ASCII words because the
references are Chinese with embedded English terms and digits, which is
where the models differ most.
"""

from __future__ import annotations

import re
import unicodedata

_PUNCT_RE = re.compile(r"[\s\W_]+", re.UNICODE)
_ASCII_WORD_RE = re.compile(r"[A-Za-z0-9]+")
_CJK = "".join(chr(code) for code in range(0x4E00, 0xA000))


def normalize_content(text: str) -> str:
    """NFKC、小写、去掉所有非字母数字字符（保留中英文与数字）。

    Content mode. Everything matched by `[\\s\\W_]` is dropped, so CJK
    characters, letters and digits survive (they are `\\w` in Unicode
    mode) while punctuation, spaces and underscores vanish. Equivalent to
    `benchmark::normalize_content` in Rust.
    """
    normalized = unicodedata.normalize("NFKC", text).lower()
    return _PUNCT_RE.sub("", normalized)


def normalize_format(text: str) -> str:
    """NFKC、小写、去掉空白（保留标点）。

    Format mode: punctuation stays so a missing or wrong mark counts as an
    error; only whitespace is removed because spacing around CJK text is
    arbitrary.
    """
    normalized = unicodedata.normalize("NFKC", text).lower()
    return re.sub(r"\s+", "", normalized)


def ascii_words(text: str) -> list[str]:
    """Runs of ASCII letters/digits, in order; the token stream WER works on."""
    return _ASCII_WORD_RE.findall(text)


def edit_distance(a: str, b: str) -> int:
    """Levenshtein distance between two sequences (strings or word lists).

    Unit cost for insert, delete and substitute; two-row DP so memory is
    O(len(b)). Works on any sequences whose elements compare with `!=`.
    """
    if a == b:
        return 0
    if not a:
        return len(b)
    if not b:
        return len(a)
    previous = list(range(len(b) + 1))
    for i, char_a in enumerate(a, 1):
        current = [i]
        for j, char_b in enumerate(b, 1):
            current.append(
                min(
                    previous[j] + 1,
                    current[j - 1] + 1,
                    previous[j - 1] + (char_a != char_b),
                )
            )
        previous = current
    return previous[-1]


def cer(reference: str, hypothesis: str, *, format_sensitive: bool = False) -> float:
    """Character error rate: edit distance / normalized reference length.

    `format_sensitive=False` (default) is the content CER the quality gate
    uses; `True` is the format CER. An empty normalized reference has
    nothing to miss and scores 0.0; the value can exceed 1.0 when the
    hypothesis is much longer than the reference.
    """
    ref = normalize_format(reference) if format_sensitive else normalize_content(reference)
    hyp = normalize_format(hypothesis) if format_sensitive else normalize_content(hypothesis)
    if not ref:
        return 0.0
    return edit_distance(ref, hyp) / len(ref)


def wer(reference: str, hypothesis: str) -> float:
    """Word error rate over the ASCII words of both texts (case-sensitive).

    A reference without ASCII words scores 0.0; callers use
    `has_ascii_words` to leave such samples out of the WER average.
    """
    ref_words = ascii_words(reference)
    if not ref_words:
        return 0.0
    hyp_words = ascii_words(hypothesis)
    return edit_distance(ref_words, hyp_words) / len(ref_words)


def has_ascii_words(reference: str) -> bool:
    """True when `wer` is meaningful for this reference."""
    return bool(ascii_words(reference))
