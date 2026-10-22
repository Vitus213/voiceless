//! `[text]` cleanup on transcripts as Qwen3-ASR actually produced them from
//! real dictation.
//!
//! Contract of `text::TextCleaner` and the `[text]` validation in
//! `config::AppConfig`: filler removal with punctuation repair, phonetic
//! hotword correction (identical and confusable syllables, untouched
//! correct spellings), exact replacements, regex rules and trailing-period
//! stripping in pipeline order, plus the config errors for a bad regex or
//! threshold and the merge of `[text].hotwords` into the engine lists.
//! Fixtures: `TextConfig` values built inline; `clean` compiles a fresh
//! cleaner per call. No fakes.

use voiceless::config::{AppConfig, TextConfig, TextReplacement, TextRule};
use voiceless::text::TextCleaner;

fn clean(config: &TextConfig, text: &str) -> String {
    TextCleaner::new(config).unwrap().clean(text)
}

/// "呃" goes wherever it appears (with the pause mark it leaves); a
/// standalone "嗯" goes; punctuation doubled by the removal is repaired.
#[test]
fn fillers_are_removed_and_punctuation_repaired() {
    let config = TextConfig::default();
    assert_eq!(
        clean(&config, "呃，我要说一个事儿，就是你看当前我这个仓库。"),
        "我要说一个事儿，就是你看当前我这个仓库。"
    );
    assert_eq!(
        clean(&config, "然后现在，呃，我会给你一份别人的简历"),
        "然后现在，我会给你一份别人的简历"
    );
    assert_eq!(
        clean(&config, "我发现现在我每次呃重新启动一次输入程序"),
        "我发现现在我每次重新启动一次输入程序"
    );
    assert_eq!(
        clean(&config, "然后现在，呃。我会给你"),
        "然后现在。我会给你"
    );
    // A standalone "嗯" goes; "嗯" inside a word or as an answer mid-text stays.
    assert_eq!(clean(&config, "你好你好你好。嗯。"), "你好你好你好。");
    assert_eq!(clean(&config, "嗯，好的"), "好的");
    // "额" is part of words like 额度 and is never treated as a filler.
    assert_eq!(clean(&config, "这个额度够吗？"), "这个额度够吗？");
}

#[test]
fn fillers_stay_when_disabled() {
    let config = TextConfig {
        remove_fillers: false,
        ..TextConfig::default()
    };
    assert_eq!(clean(&config, "呃，我要说"), "呃，我要说");
}

#[test]
fn sound_alike_spans_become_the_hotword() {
    let config = TextConfig {
        hotwords: vec!["张三丰".to_owned(), "通义千问".to_owned()],
        ..TextConfig::default()
    };
    // Same syllables, different characters.
    assert_eq!(clean(&config, "我去找张三峰聊聊"), "我去找张三丰聊聊");
    // A confusable final (wen/weng) still matches a four-syllable word.
    assert_eq!(clean(&config, "用通义千翁试试"), "用通义千问试试");
    // Unrelated text and the correct word are untouched.
    assert_eq!(clean(&config, "张三丰和李四"), "张三丰和李四");
    assert_eq!(clean(&config, "今天天气很好"), "今天天气很好");
}

/// Replacement, then regex rule, then one trailing period, in that order.
#[test]
fn replacements_rules_and_trailing_period() {
    let config = TextConfig {
        replacements: vec![TextReplacement {
            from: "receive test".to_owned(),
            to: "resume text".to_owned(),
        }],
        rules: vec![TextRule {
            pattern: r"(\d+)点(\d+)分".to_owned(),
            replace: "$1:$2".to_owned(),
        }],
        strip_trailing_period: true,
        ..TextConfig::default()
    };
    assert_eq!(
        clean(&config, "就写第二份，就 receive test。"),
        "就写第二份，就 resume text"
    );
    assert_eq!(clean(&config, "现在是5点23分。"), "现在是5:23");
}

#[test]
fn invalid_rules_and_thresholds_are_config_errors() {
    assert!(AppConfig::from_toml("[[text.rules]]\npattern = \"(\"\nreplace = \"\"\n").is_err());
    assert!(AppConfig::from_toml("[text]\nhotword_threshold = 0.2\n").is_err());
    let config = AppConfig::from_toml(
        "[text]\nhotwords = [\"Voiceless\"]\n\n[local]\nhotwords = [\"NixOS\"]\n",
    )
    .unwrap();
    // One list biases the recognizers too.
    assert_eq!(config.local_engine().hotwords, vec!["NixOS", "Voiceless"]);
    assert_eq!(config.doubao_engine().hotwords, vec!["Voiceless"]);
}
