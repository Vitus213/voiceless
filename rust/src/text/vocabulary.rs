use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, TextConfig, TextReplacement, TextRule};
use crate::offline_asr::OfflineFamily;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Terms {
    pub hotwords: Vec<String>,
    pub replacements: Vec<TextReplacement>,
    pub rules: Vec<TextRule>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedVocabulary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub terms: Terms,
}

impl ScopedVocabulary {
    fn specificity(&self) -> u8 {
        u8::from(self.mode.is_some()) + 2 * u8::from(self.project.is_some())
    }

    fn matches(&self, mode: Option<&str>, project: Option<&str>) -> bool {
        self.mode.as_deref().is_none_or(|scope| Some(scope) == mode)
            && self
                .project
                .as_deref()
                .is_none_or(|scope| Some(scope) == project)
    }
}

pub(crate) fn validate(scopes: &[ScopedVocabulary]) -> Result<(), String> {
    for (index, scope) in scopes.iter().enumerate() {
        if scope.specificity() == 0
            || [scope.mode.as_deref(), scope.project.as_deref()]
                .into_iter()
                .flatten()
                .any(|value| value.is_empty() || value != value.trim())
        {
            return Err(
                "each scoped vocabulary needs a mode or project without surrounding whitespace"
                    .to_owned(),
            );
        }
        if scopes[..index]
            .iter()
            .any(|other| other.mode == scope.mode && other.project == scope.project)
        {
            return Err("duplicate vocabulary for the same mode and project".to_owned());
        }
        scope.terms.validate()?;
    }
    Ok(())
}

pub(crate) fn effective<'a>(
    config: &'a TextConfig,
    mode: Option<&str>,
    project: Option<&str>,
) -> Cow<'a, TextConfig> {
    let mut effective = None;
    // Project entries override mode entries, and a combined scope can refine both.
    for specificity in 1..=3 {
        for scope in config
            .scopes
            .iter()
            .filter(|scope| scope.specificity() == specificity && scope.matches(mode, project))
        {
            let text = effective.get_or_insert_with(|| TextConfig {
                remove_fillers: config.remove_fillers,
                strip_trailing_period: config.strip_trailing_period,
                hotword_threshold: config.hotword_threshold,
                hotwords: config.hotwords.clone(),
                replacements: config.replacements.clone(),
                rules: config.rules.clone(),
                scopes: Vec::new(),
            });
            combine(
                &mut text.hotwords,
                &scope.terms.hotwords,
                String::as_str,
                None,
            );
            combine(
                &mut text.replacements,
                &scope.terms.replacements,
                |entry| &entry.from,
                None,
            );
            combine(
                &mut text.rules,
                &scope.terms.rules,
                |entry| &entry.pattern,
                None,
            );
        }
    }
    effective.map_or(Cow::Borrowed(config), Cow::Owned)
}

pub(crate) fn recognition_bias(config: &AppConfig) -> Option<bool> {
    match config.asr.streaming_backend.as_str() {
        "doubao-realtime" => Some(true),
        "local-offline" => match OfflineFamily::for_model(&config.local.model).ok()? {
            OfflineFamily::Qwen3Asr => Some(true),
            OfflineFamily::SenseVoice => Some(false),
            OfflineFamily::Gguf => {
                if !cfg!(feature = "transcribe") {
                    Some(false)
                } else if config.local.model.starts_with("qwen3-asr-") {
                    Some(true)
                } else {
                    None
                }
            }
        },
        _ => Some(false),
    }
}

#[derive(Default, Serialize)]
pub(crate) struct MergeReport {
    added: usize,
    duplicates: usize,
    conflicts: Vec<Conflict>,
}

#[derive(Serialize)]
struct Conflict {
    kind: &'static str,
    key: String,
}

impl Terms {
    fn validate(&self) -> Result<(), String> {
        if self.hotwords.iter().any(|word| word.trim().is_empty())
            || self.replacements.iter().any(|entry| entry.from.is_empty())
            || self.rules.iter().any(|entry| entry.pattern.is_empty())
        {
            return Err(
                "vocabulary terms, replacement keys and regex patterns cannot be empty".to_owned(),
            );
        }
        for entry in &self.rules {
            regex::Regex::new(&entry.pattern)
                .map_err(|error| format!("text.rules pattern {:?}: {error}", entry.pattern))?;
        }
        Ok(())
    }

    pub(crate) fn import(&mut self, mut incoming: Self) -> Result<MergeReport, String> {
        if incoming.hotwords.len() + incoming.replacements.len() + incoming.rules.len() > 1024 {
            return Err("import at most 1024 vocabulary entries at a time".to_owned());
        }
        incoming
            .hotwords
            .iter_mut()
            .for_each(|word| *word = word.trim().to_owned());
        incoming.validate()?;
        let mut stats = MergeReport::default();
        combine(
            &mut self.hotwords,
            &incoming.hotwords,
            String::as_str,
            Some((&mut stats, "hotword")),
        );
        combine(
            &mut self.replacements,
            &incoming.replacements,
            |entry| &entry.from,
            Some((&mut stats, "replacement")),
        );
        combine(
            &mut self.rules,
            &incoming.rules,
            |entry| &entry.pattern,
            Some((&mut stats, "regex")),
        );
        Ok(stats)
    }
}

fn combine<T: Clone + PartialEq>(
    entries: &mut Vec<T>,
    incoming: &[T],
    key: impl Fn(&T) -> &str,
    mut report: Option<(&mut MergeReport, &'static str)>,
) {
    for item in incoming {
        if let Some(previous) = entries.iter_mut().find(|entry| key(entry) == key(item)) {
            if let Some((report, kind)) = &mut report {
                if previous == item {
                    report.duplicates += 1;
                } else {
                    report.conflicts.push(Conflict {
                        kind,
                        key: key(item).to_owned(),
                    });
                }
            } else if previous != item {
                *previous = item.clone();
            }
        } else {
            entries.push(item.clone());
            if let Some((report, _)) = &mut report {
                report.added += 1;
            }
        }
    }
}
