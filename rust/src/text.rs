//! Final-text cleanup applied to every recognized utterance before mode
//! processing and injection (ideas from CapsWriter-Offline and voxtype):
//! filler removal, phonetic hotword correction, exact and regex
//! replacements, and optional trailing-period stripping.
//!
//! Configured by `[text]` (`config::TextConfig`). `config::AppConfig`
//! builds a `TextCleaner` during validation so a bad regex rule fails at
//! load time; `stream::build_pipeline` builds the one a session uses and
//! `coordinator::run_session` runs `clean` through `TextProcessor::cleanup`
//! on the authoritative transcript, in every mode, before the LLM step and
//! before history. The console's vocabulary page previews the same `clean`
//! on sample text, its re-transcribe action cleans the new decode, and the
//! history "fix" action turns a correction into a replacement or hotword
//! that reaches this module through `PUT /api/config`.
//!
//! Order inside `clean`: fillers plus punctuation repair (when enabled),
//! phonetic hotword correction, exact replacements in config order, regex
//! rules in config order, one trailing period, trim. Each step sees the
//! output of the previous one, so a replacement can target text a hotword
//! produced and a rule can target text a replacement produced.
//!
//! The phonetic step is the non-obvious part (see `correct_hotwords`,
//! `similarity`, `normalize`): Chinese characters become tone-less pinyin
//! syllables; a window of exactly a hotword's length is compared syllable
//! by syllable (identical 1.0, a known confusion 0.8, anything else 0.0)
//! and replaced by the hotword when the mean reaches `hotword_threshold`.
//! Only all-Chinese hotwords of two or more characters take part: a lone
//! syllable has too many homophones to rewrite safely and non-Chinese
//! characters have no pinyin. Other hotwords still bias the recognizers.

use pinyin::ToPinyin;
use regex::Regex;

use crate::config::TextConfig;

/// Compiled `[text]` rules; built once per session. Immutable afterwards
/// and shareable across threads. Construction fails only for a `text.rules`
/// pattern that does not compile.
pub struct TextCleaner {
    remove_fillers: bool,
    strip_trailing_period: bool,
    /// `(from, to)` literal pairs in config order.
    replacements: Vec<(String, String)>,
    /// Compiled `(pattern, replacement)` pairs in config order.
    rules: Vec<(Regex, String)>,
    /// Only the hotwords eligible for phonetic correction.
    hotwords: Vec<Hotword>,
    /// `[text].hotword_threshold`, validated to 0.5..=1.0.
    threshold: f32,
    /// Matches "呃" runs (plus one trailing pause mark) anywhere.
    filler: Regex,
    /// Matches "嗯" runs only as a standalone interjection.
    lone_filler: Regex,
}

/// A hotword eligible for phonetic correction: its characters and their
/// tone-less pinyin syllables, one per character.
struct Hotword {
    /// The word as configured (what gets inserted).
    text: String,
    /// `text` as characters; its length is the window size.
    chars: Vec<char>,
    /// Plain pinyin per character, same length as `chars`.
    syllables: Vec<&'static str>,
}

impl TextCleaner {
    /// Compile the rules. The only error is a regex pattern that does not
    /// compile (the message names the pattern). Hotwords with any
    /// non-Chinese character or fewer than two characters are left out of
    /// the phonetic list silently; they still reach the recognizers through
    /// `AppConfig::local_engine` and `AppConfig::doubao_engine`.
    pub fn new(config: &TextConfig) -> Result<Self, String> {
        let rules = config
            .rules
            .iter()
            .map(|rule| {
                Regex::new(&rule.pattern)
                    .map(|regex| (regex, rule.replace.clone()))
                    .map_err(|error| format!("text.rules pattern {:?}: {error}", rule.pattern))
            })
            .collect::<Result<_, _>>()?;
        let hotwords = config
            .hotwords
            .iter()
            .filter_map(|word| {
                let chars: Vec<char> = word.chars().collect();
                // Syllable extraction, done once per hotword: one tone-less
                // pinyin syllable per character from the `pinyin` crate's
                // single-reading table (`plain()` drops the tone marks, so
                // 问/闻/文 all become "wen"; a character with several
                // readings contributes only its primary one). Collecting
                // into `Option<Vec<_>>` turns a single character without a
                // reading (Latin letter, digit, punctuation) into `None`.
                let syllables: Option<Vec<_>> = chars
                    .iter()
                    .map(|ch| ch.to_pinyin().map(|pinyin| pinyin.plain()))
                    .collect();
                // Phonetic matching needs an all-Chinese word of two or more
                // characters; other hotwords only bias the recognizer.
                syllables
                    .filter(|syllables| syllables.len() >= 2)
                    .map(|syllables| Hotword {
                        text: word.clone(),
                        chars,
                        syllables,
                    })
            })
            .collect();
        Ok(Self {
            remove_fillers: config.remove_fillers,
            strip_trailing_period: config.strip_trailing_period,
            replacements: config
                .replacements
                .iter()
                .map(|replacement| (replacement.from.clone(), replacement.to.clone()))
                .collect(),
            rules,
            hotwords,
            threshold: config.hotword_threshold,
            // "呃" is a filler wherever it appears; drop a comma it leaves.
            filler: Regex::new("呃+[，、,]?").expect("static regex"),
            // "嗯" only as a standalone interjection (it can mean "yes").
            lone_filler: Regex::new("(^|[。！？，、])嗯+[，。、,]").expect("static regex"),
        })
    }

    /// Apply every step to one final transcript and return the trimmed
    /// result (see the module doc for the order). Pure: no I/O, no state;
    /// an empty input yields an empty string. The coordinator calls it on
    /// the final transcript only.
    pub fn clean(&self, text: &str) -> String {
        let mut text = text.to_owned();
        if self.remove_fillers {
            text = self.filler.replace_all(&text, "").into_owned();
            // `$1` keeps the anchor (start of text or the punctuation before
            // 嗯); the mark after 嗯 goes with it, e.g. "嗯，好的" -> "好的".
            text = self.lone_filler.replace_all(&text, "$1").into_owned();
            text = tidy_punctuation(&text);
        }
        text = self.correct_hotwords(&text);
        for (from, to) in &self.replacements {
            // `str::replace` with an empty needle would insert `to` between
            // every character.
            if !from.is_empty() {
                text = text.replace(from.as_str(), to);
            }
        }
        for (regex, replace) in &self.rules {
            text = regex.replace_all(&text, replace.as_str()).into_owned();
        }
        // One period only: a deliberate "……" or "..." is not touched beyond
        // its last character.
        if self.strip_trailing_period {
            if let Some(stripped) = text.strip_suffix('。').or_else(|| text.strip_suffix('.')) {
                text = stripped.to_owned();
            }
        }
        text.trim().to_owned()
    }

    /// Replace spans that sound like a hotword (same pinyin syllable count,
    /// similarity ≥ threshold) with the hotword, left to right.
    ///
    /// Algorithm:
    /// 1. Map every character of `text` to its tone-less pinyin syllable
    ///    once (`None` for a non-Chinese character), so each comparison
    ///    below is a slice lookup.
    /// 2. Walk the text by character. At each position consider every
    ///    hotword that still fits; the window is exactly as long as the
    ///    hotword, so a hotword only ever replaces a span of its own length
    ///    (no insertions or deletions).
    /// 3. A window that already spells the hotword scores 2.0, above any
    ///    phonetic score: the correct spelling is kept and consumed, which
    ///    also stops a shorter hotword from rewriting part of it.
    /// 4. Otherwise `similarity` averages the per-syllable scores; a window
    ///    containing a non-Chinese character never matches, and scores
    ///    below the threshold are dropped.
    /// 5. The best-scoring hotword wins (ties go to the last one in config
    ///    order, as `Iterator::max_by` returns the last maximum) and the
    ///    cursor jumps past the window; with no candidate the character is
    ///    copied and the cursor moves one step. Replaced spans therefore
    ///    never overlap and the output is never rescanned.
    fn correct_hotwords(&self, text: &str) -> String {
        if self.hotwords.is_empty() {
            return text.to_owned();
        }
        let chars: Vec<char> = text.chars().collect();
        // Step 1: syllables of the transcript, aligned with `chars`.
        let syllables: Vec<Option<&'static str>> = chars
            .iter()
            .map(|ch| ch.to_pinyin().map(|pinyin| pinyin.plain()))
            .collect();
        let mut out = String::with_capacity(text.len());
        let mut index = 0;
        while index < chars.len() {
            let best = self
                .hotwords
                .iter()
                // Step 2: only hotwords whose window fits before the end.
                .filter(|word| index + word.chars.len() <= chars.len())
                .filter_map(|word| {
                    let span = &chars[index..index + word.chars.len()];
                    // Step 3: exact spelling beats every phonetic candidate.
                    if span == word.chars.as_slice() {
                        return Some((word, 2.0)); // already correct: keep it
                    }
                    // Step 4: mean syllable similarity against the threshold.
                    let window = &syllables[index..index + word.chars.len()];
                    let score = similarity(window, &word.syllables)?;
                    (score >= self.threshold).then_some((word, score))
                })
                .max_by(|a, b| a.1.total_cmp(&b.1));
            // Step 5: emit the winner and skip its span, or copy one char.
            match best {
                Some((word, _)) => {
                    out.push_str(&word.text);
                    index += word.chars.len();
                }
                None => {
                    out.push(chars[index]);
                    index += 1;
                }
            }
        }
        out
    }
}

/// Mean per-syllable similarity; `None` when the window has a non-Chinese
/// character. Exact syllables score 1, common confusions (zh/z, n/l,
/// -ng/-n, …) 0.8.
///
/// `window` and `target` have the same length by construction, and tones
/// were already discarded (both sides are `plain()` pinyin). The 0.8
/// weight and the mean make `hotword_threshold` a simple knob: with k
/// confused syllables out of n the score is 1 - 0.2·k/n, so the default
/// 0.9 accepts k ≤ n/2 (one confusion in a two- or three-syllable word) and
/// 1.0 demands identical syllables. A syllable that differs beyond a known
/// confusion scores 0, which at the default rules the window out (one
/// unrelated syllable in a two-syllable word averages 0.5).
fn similarity(window: &[Option<&'static str>], target: &[&'static str]) -> Option<f32> {
    let mut total = 0.0;
    for (heard, wanted) in window.iter().zip(target) {
        // Any non-Chinese character in the window disqualifies it.
        let heard = (*heard)?;
        total += if heard == *wanted {
            1.0
        } else if normalize(heard) == normalize(wanted) {
            // Same syllable once the confusable distinctions are collapsed.
            0.8
        } else {
            0.0
        };
    }
    Some(total / target.len() as f32)
}

/// Collapse the confusable pinyin distinctions speakers and recognizers
/// blur: retroflex initials, n/l, f/h, and front/back nasal finals.
///
/// Both syllables are normalized and then compared for equality, so the
/// mapping only has to be consistent, not phonetically meaningful (`hua`
/// becomes `fua`, which is no syllable, yet `hua` and `fa` still differ).
/// Each rule runs once, in order, on the current string; a syllable has at
/// most one initial and one final, so the rules never interact.
fn normalize(syllable: &str) -> String {
    let mut s = syllable.to_owned();
    // Retroflex vs. dental initials (zh/z, ch/c, sh/s): the merger of many
    // southern Mandarin accents.
    for (from, to) in [("zh", "z"), ("ch", "c"), ("sh", "s")] {
        if let Some(rest) = s.strip_prefix(from) {
            s = format!("{to}{rest}");
        }
    }
    // n/l initial merger (南/兰); both sides are mapped to `n`.
    if let Some(rest) = s.strip_prefix('l') {
        s = format!("n{rest}");
    }
    // h/f initial merger (湖/福); a bare `h` is left alone.
    if let Some(rest) = s.strip_prefix('h') {
        if !rest.is_empty() {
            s = format!("f{rest}");
        }
    }
    // Front/back nasal finals (an/ang, en/eng, in/ing), e.g. 问 `wen` heard
    // as 翁 `weng`.
    for (from, to) in [("ang", "an"), ("eng", "en"), ("ing", "in")] {
        if let Some(stem) = s.strip_suffix(from) {
            s = format!("{stem}{to}");
        }
    }
    s
}

/// Fix punctuation left behind by removed fillers.
///
/// Single pass; the last character already emitted decides: punctuation at
/// the very start is dropped, a pause mark after a pause or a stop is
/// dropped, and a stop after a pause replaces the pause. Two stops in a
/// row ("。。") are left alone. Runs only when `remove_fillers` is on.
fn tidy_punctuation(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        let pause = matches!(ch, '，' | '、' | ',');
        let stop = matches!(ch, '。' | '！' | '？');
        match out.chars().last() {
            // Nothing to separate at the very start.
            None if pause || stop => continue,
            // "，，" -> "，"; "。，" -> "。"
            Some(prev) if pause && matches!(prev, '，' | '、' | ',' | '。' | '！' | '？') => {
                continue
            }
            // "，。" -> "。"
            Some(prev) if stop && matches!(prev, '，' | '、' | ',') => {
                out.pop();
            }
            _ => {}
        }
        out.push(ch);
    }
    out
}
