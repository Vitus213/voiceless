//! Final-text cleanup applied to every recognized utterance before mode
//! processing and injection (ideas from CapsWriter-Offline and voxtype):
//! filler removal, phonetic hotword correction, exact and regex
//! replacements, and optional trailing-period stripping.

use pinyin::ToPinyin;
use regex::Regex;

use crate::config::TextConfig;

/// Compiled `[text]` rules; built once per session.
pub struct TextCleaner {
    remove_fillers: bool,
    strip_trailing_period: bool,
    replacements: Vec<(String, String)>,
    rules: Vec<(Regex, String)>,
    hotwords: Vec<Hotword>,
    threshold: f32,
    filler: Regex,
    lone_filler: Regex,
}

struct Hotword {
    text: String,
    chars: Vec<char>,
    syllables: Vec<&'static str>,
}

impl TextCleaner {
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

    pub fn clean(&self, text: &str) -> String {
        let mut text = text.to_owned();
        if self.remove_fillers {
            text = self.filler.replace_all(&text, "").into_owned();
            text = self.lone_filler.replace_all(&text, "$1").into_owned();
            text = tidy_punctuation(&text);
        }
        text = self.correct_hotwords(&text);
        for (from, to) in &self.replacements {
            if !from.is_empty() {
                text = text.replace(from.as_str(), to);
            }
        }
        for (regex, replace) in &self.rules {
            text = regex.replace_all(&text, replace.as_str()).into_owned();
        }
        if self.strip_trailing_period {
            if let Some(stripped) = text.strip_suffix('。').or_else(|| text.strip_suffix('.')) {
                text = stripped.to_owned();
            }
        }
        text.trim().to_owned()
    }

    /// Replace spans that sound like a hotword (same pinyin syllable count,
    /// similarity ≥ threshold) with the hotword, left to right.
    fn correct_hotwords(&self, text: &str) -> String {
        if self.hotwords.is_empty() {
            return text.to_owned();
        }
        let chars: Vec<char> = text.chars().collect();
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
                .filter(|word| index + word.chars.len() <= chars.len())
                .filter_map(|word| {
                    let span = &chars[index..index + word.chars.len()];
                    if span == word.chars.as_slice() {
                        return Some((word, 2.0)); // already correct: keep it
                    }
                    let window = &syllables[index..index + word.chars.len()];
                    let score = similarity(window, &word.syllables)?;
                    (score >= self.threshold).then_some((word, score))
                })
                .max_by(|a, b| a.1.total_cmp(&b.1));
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
fn similarity(window: &[Option<&'static str>], target: &[&'static str]) -> Option<f32> {
    let mut total = 0.0;
    for (heard, wanted) in window.iter().zip(target) {
        let heard = (*heard)?;
        total += if heard == *wanted {
            1.0
        } else if normalize(heard) == normalize(wanted) {
            0.8
        } else {
            0.0
        };
    }
    Some(total / target.len() as f32)
}

/// Collapse the confusable pinyin distinctions speakers and recognizers
/// blur: retroflex initials, n/l, f/h, and front/back nasal finals.
fn normalize(syllable: &str) -> String {
    let mut s = syllable.to_owned();
    for (from, to) in [("zh", "z"), ("ch", "c"), ("sh", "s")] {
        if let Some(rest) = s.strip_prefix(from) {
            s = format!("{to}{rest}");
        }
    }
    if let Some(rest) = s.strip_prefix('l') {
        s = format!("n{rest}");
    }
    if let Some(rest) = s.strip_prefix('h') {
        if !rest.is_empty() {
            s = format!("f{rest}");
        }
    }
    for (from, to) in [("ang", "an"), ("eng", "en"), ("ing", "in")] {
        if let Some(stem) = s.strip_suffix(from) {
            s = format!("{stem}{to}");
        }
    }
    s
}

/// Fix punctuation left behind by removed fillers.
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
