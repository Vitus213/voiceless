//! Shared output execution for dictation and local preview.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::config::AppConfig;
use crate::modes::{Mode, ModesRepository};
use crate::processing::{self, ChatProcessor, UreqPoster};
use crate::text::TextCleaner;

mod script;
pub use script::{run_script_worker, Script, SCRIPT_WORKER_ARG};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Step {
    Fillers,
    Hotwords,
    Replace,
    Regex,
    Ending,
    Trim,
    Model,
    Script,
}

pub(crate) const CLEANUP: &[Step] = &[
    Step::Fillers,
    Step::Hotwords,
    Step::Replace,
    Step::Regex,
    Step::Ending,
    Step::Trim,
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    pub rules: Vec<ApplicationRule>,
    pub pipelines: Vec<ModePipeline>,
    pub allow_opencode: bool,
    pub allow_site_context: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationRule {
    #[serde(default)]
    pub app_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub mode: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
}

fn enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModePipeline {
    pub mode: String,
    pub steps: Vec<Step>,
    #[serde(default)]
    pub allow_model: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<Script>,
}

impl OutputConfig {
    pub(crate) fn validate(&self) -> Result<(), String> {
        for rule in &self.rules {
            if (rule.app_id.trim().is_empty() && rule.site.is_none()) || rule.mode.trim().is_empty()
            {
                return Err("each output rule needs an application or site and a mode".to_owned());
            }
            if rule
                .site
                .as_deref()
                .is_some_and(|site| hostname(site).is_none())
            {
                return Err("output site rules need a hostname without a path or port".to_owned());
            }
            if rule
                .project
                .as_deref()
                .is_some_and(|project| project.trim().is_empty())
            {
                return Err("output project names cannot be blank".to_owned());
            }
        }
        for (index, pipeline) in self.pipelines.iter().enumerate() {
            if pipeline.mode.trim().is_empty() || pipeline.mode == crate::modes::RAW_MODE_ID {
                return Err("each output pipeline needs an editable mode".to_owned());
            }
            if self.pipelines[..index]
                .iter()
                .any(|other| other.mode == pipeline.mode)
            {
                return Err(format!("duplicate output pipeline for {}", pipeline.mode));
            }
            validate_steps(&pipeline.mode, &pipeline.steps)?;
            if let Some(script) = &pipeline.script {
                script.validate()?;
            }
        }
        Ok(())
    }
}

fn validate_steps(mode: &str, steps: &[Step]) -> Result<(), String> {
    if mode == "quick" && steps.contains(&Step::Model) {
        return Err("quick mode cannot call a text processing model".to_owned());
    }
    for (index, step) in steps.iter().enumerate() {
        if steps[..index].contains(step) {
            return Err(format!("duplicate step {step:?} in {mode}"));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModeSource {
    Manual,
    ApplicationRule,
    SiteRule,
    Default,
}

#[derive(Debug, Clone, Serialize)]
pub struct Selection {
    pub mode: String,
    pub source: ModeSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_source: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_source: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

#[derive(Default)]
pub(crate) struct Context<'a> {
    pub app_id: Option<&'a str>,
    pub app_source: Option<&'static str>,
    pub site: Option<&'a str>,
    pub site_source: Option<&'static str>,
    pub project: Option<&'a str>,
}

fn hostname(site: &str) -> Option<&str> {
    let host = site.trim().trim_end_matches('.');
    let valid = host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        });
    valid.then_some(host)
}

fn matches(rule: &ApplicationRule, context: &Context<'_>) -> bool {
    let app = rule.app_id.trim();
    let app_matches = app.is_empty()
        || context
            .app_id
            .is_some_and(|id| app.eq_ignore_ascii_case(id));
    let site_matches = rule.site.as_deref().is_none_or(|site| {
        hostname(site)
            .zip(context.site)
            .is_some_and(|(rule, site)| rule.eq_ignore_ascii_case(site))
    });
    rule.enabled && app_matches && site_matches
}

fn select<'a>(
    config: &'a OutputConfig,
    modes: &'a ModesRepository,
    requested: &str,
    default: &str,
    context: &Context<'_>,
) -> Result<(&'a Mode, ModeSource, Option<&'a str>), String> {
    let rule = config.rules.iter().find_map(|rule| {
        // A deleted mode does not disable dictation for this target.
        matches(rule, context)
            .then(|| modes.get(&rule.mode).ok().map(|mode| (rule, mode)))
            .flatten()
    });
    if !requested.trim().is_empty() {
        return modes
            .resolve(Some(requested))
            .map(|mode| {
                (
                    mode,
                    ModeSource::Manual,
                    rule.and_then(|(rule, _)| rule.project.as_deref()),
                )
            })
            .map_err(|error| error.to_string());
    }
    if let Some((rule, mode)) = rule {
        let source = if rule.site.is_some() {
            ModeSource::SiteRule
        } else {
            ModeSource::ApplicationRule
        };
        return Ok((mode, source, rule.project.as_deref()));
    }
    modes
        .resolve(Some(default))
        .map(|mode| (mode, ModeSource::Default, None))
        .map_err(|error| error.to_string())
}

impl Context<'_> {
    fn selection(&self, mode: &str, source: ModeSource, project: Option<&str>) -> Selection {
        Selection {
            mode: mode.to_owned(),
            source,
            app_id: self.app_id.map(str::to_owned),
            app_source: self.app_id.and(self.app_source),
            site: self.site.map(str::to_owned),
            site_source: self.site.and(self.site_source),
            project: self.project.or(project).map(str::to_owned),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StepReceipt {
    pub step: Step,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Trace {
    pub selection: Selection,
    pub steps: Vec<StepReceipt>,
}

#[derive(Debug, Serialize)]
pub struct Preview {
    pub text: String,
    pub trace: Trace,
    pub error: Option<String>,
}

/// Prepared at activation. A session cannot pick up edits from later requests.
pub(crate) struct Pipeline {
    selection: Selection,
    steps: Vec<Step>,
    cleaner: Option<TextCleaner>,
    model: Option<ChatProcessor<UreqPoster>>,
    prompt: String,
    allow_model: bool,
    script: Option<Script>,
    scoped_hotwords: Option<Vec<String>>,
}

impl Pipeline {
    pub(crate) fn prepare(
        config: &AppConfig,
        requested: &str,
        default: &str,
        mut context: Context<'_>,
    ) -> Result<Self, String> {
        context.app_id = context.app_id.map(str::trim).filter(|id| !id.is_empty());
        context.project = context
            .project
            .map(str::trim)
            .filter(|project| !project.is_empty());
        context.site = if config.output.allow_site_context {
            context
                .site
                .map(str::trim)
                .filter(|site| !site.is_empty())
                .map(|site| {
                    hostname(site).ok_or("site context needs a hostname without a path or port")
                })
                .transpose()?
        } else {
            None
        };
        if requested == crate::modes::RAW_MODE_ID {
            return Ok(Self {
                selection: context.selection(requested, ModeSource::Manual, None),
                steps: Vec::new(),
                cleaner: None,
                model: None,
                prompt: String::new(),
                allow_model: false,
                script: None,
                scoped_hotwords: None,
            });
        }
        let modes = ModesRepository::open(crate::stream::modes_path())
            .map_err(|error| error.to_string())?;
        let (mode, source, project) = select(&config.output, &modes, requested, default, &context)?;
        let (steps, allow_model, script) = match config
            .output
            .pipelines
            .iter()
            .find(|item| item.mode == mode.id)
        {
            Some(pipeline) => (
                pipeline.steps.clone(),
                pipeline.allow_model,
                pipeline.script.clone(),
            ),
            None => {
                let mut steps = CLEANUP.to_vec();
                if mode.id != "quick" {
                    steps.push(Step::Model);
                }
                // Existing manually selected and default modes retain their
                // processing behavior. New automatic rules require a grant.
                (
                    steps,
                    matches!(source, ModeSource::Manual | ModeSource::Default),
                    None,
                )
            }
        };
        let model = if allow_model && steps.contains(&Step::Model) {
            processing::from_config(&config.processing).map_err(|error| error.to_string())?
        } else {
            None
        };
        let prompt = if mode.id == crate::modes::PROMPT_OPTIMIZE_ID
            && !config.processing.prompt.trim().is_empty()
        {
            config.processing.prompt.clone()
        } else {
            mode.prompt.clone()
        };
        let selection = context.selection(&mode.id, source, project);
        let text = crate::text::vocabulary::effective(
            &config.text,
            Some(&selection.mode),
            selection.project.as_deref(),
        );
        let cleaner = TextCleaner::new(&text)?;
        let scoped_hotwords = match text {
            std::borrow::Cow::Owned(text) if text.hotwords != config.text.hotwords => {
                Some(text.hotwords)
            }
            _ => None,
        };
        Ok(Self {
            selection,
            steps,
            cleaner: Some(cleaner),
            model,
            prompt,
            allow_model,
            script,
            scoped_hotwords,
        })
    }

    pub(crate) fn selection(&self) -> &Selection {
        &self.selection
    }

    pub(crate) fn scoped_hotwords(&self) -> Option<&[String]> {
        self.scoped_hotwords.as_deref()
    }

    pub(crate) fn uses_model(&self) -> bool {
        self.steps.contains(&Step::Model)
    }

    pub(crate) async fn run(&self, input: &str, preview: bool, send_model: bool) -> Preview {
        let mut text = input.to_owned();
        let mut trace = Trace {
            selection: self.selection.clone(),
            steps: Vec::with_capacity(self.steps.len()),
        };
        let mut error = None;
        for &step in &self.steps {
            let started = Instant::now();
            let before = preview.then(|| text.clone());
            let outcome = match step {
                Step::Model => self
                    .model_step(&text, send_model)
                    .await
                    .map(|result| text = result),
                Step::Script => match &self.script {
                    Some(script) => script.run(&text).await.map(|result| text = result),
                    None => Err("this mode has no output script configured".to_owned()),
                },
                _ => {
                    self.cleaner
                        .as_ref()
                        .expect("configured local step has a cleaner")
                        .apply(step, &mut text);
                    Ok(())
                }
            };
            let receipt = StepReceipt {
                step,
                elapsed_ms: started.elapsed().as_millis() as u64,
                before,
                after: preview.then(|| text.clone()),
                error: outcome.err(),
            };
            error.clone_from(&receipt.error);
            trace.steps.push(receipt);
            if error.is_some() {
                break;
            }
        }
        Preview { text, trace, error }
    }

    async fn model_step(&self, input: &str, send_model: bool) -> Result<String, String> {
        if !self.allow_model || !send_model {
            return Err("the text processing model needs explicit permission".to_owned());
        }
        let model = self
            .model
            .clone()
            .ok_or("no text processing provider configured")?;
        let prompt = crate::modes::render_template(&self.prompt, input, "", "");
        tokio::task::spawn_blocking(move || model.process(&prompt))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
    }
}
