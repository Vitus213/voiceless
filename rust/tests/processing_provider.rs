use std::time::Duration;

use voiceless::processing::{ChatProcessor, HttpPoster, ProcessingError};

struct ScriptedPoster {
    status: u16,
    body: String,
    last_url: parking_lot::Mutex<String>,
    last_body: parking_lot::Mutex<String>,
    last_bearer: parking_lot::Mutex<Option<String>>,
}

impl ScriptedPoster {
    fn new(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            body: body.to_string(),
            last_url: parking_lot::Mutex::new(String::new()),
            last_body: parking_lot::Mutex::new(String::new()),
            last_bearer: parking_lot::Mutex::new(None),
        }
    }
}

impl HttpPoster for ScriptedPoster {
    fn post_json(
        &self,
        url: &str,
        body: &str,
        bearer: Option<&str>,
        _timeout: Duration,
    ) -> Result<(u16, String), ProcessingError> {
        *self.last_url.lock() = url.to_owned();
        *self.last_body.lock() = body.to_owned();
        *self.last_bearer.lock() = bearer.map(ToOwned::to_owned);
        Ok((self.status, self.body.clone()))
    }
}

#[test]
fn openai_compatible_extracts_choice_content() {
    let poster = ScriptedPoster::new(
        200,
        serde_json::json!({"choices": [{"message": {"content": "处理结果"}}]}),
    );
    let processor = ChatProcessor::new(
        "openai-compatible".to_owned(),
        "https://api.example.com/v1/".to_owned(),
        "model-x".to_owned(),
        "sk-secret".to_owned(),
        Duration::from_secs(5),
        poster,
    );

    let text = processor.process("润色：原文").expect("process");
    assert_eq!(text, "处理结果");
    assert_eq!(
        *processor.poster_ref().last_url.lock(),
        "https://api.example.com/v1/chat/completions"
    );
    let body: serde_json::Value =
        serde_json::from_str(&processor.poster_ref().last_body.lock()).expect("body json");
    assert_eq!(body["model"], "model-x");
    assert_eq!(body["messages"][0]["content"], "润色：原文");
    assert_eq!(
        processor.poster_ref().last_bearer.lock().as_deref(),
        Some("sk-secret")
    );
}

#[test]
fn ollama_reads_message_content_with_stream_disabled() {
    let poster = ScriptedPoster::new(
        200,
        serde_json::json!({"message": {"content": "ollama 结果"}}),
    );
    let processor = ChatProcessor::new(
        "ollama".to_owned(),
        "http://localhost:11434".to_owned(),
        "qwen2.5".to_owned(),
        String::new(),
        Duration::from_secs(5),
        poster,
    );

    let text = processor.process("prompt").expect("process");
    assert_eq!(text, "ollama 结果");
    assert_eq!(
        *processor.poster_ref().last_url.lock(),
        "http://localhost:11434/api/chat"
    );
    let body: serde_json::Value =
        serde_json::from_str(&processor.poster_ref().last_body.lock()).expect("body json");
    assert_eq!(body["stream"], false);
    assert!(processor.poster_ref().last_bearer.lock().is_none());
}

#[test]
fn non_success_status_and_empty_content_are_errors() {
    let poster = ScriptedPoster::new(401, serde_json::json!({"error": "unauthorized"}));
    let processor = ChatProcessor::new(
        "openai-compatible".to_owned(),
        "https://api.example.com".to_owned(),
        "model".to_owned(),
        "bad".to_owned(),
        Duration::from_secs(5),
        poster,
    );
    assert!(matches!(
        processor.process("x"),
        Err(ProcessingError::Status(401))
    ));

    let poster = ScriptedPoster::new(
        200,
        serde_json::json!({"choices": [{"message": {"content": "   "}}]}),
    );
    let processor = ChatProcessor::new(
        "openai-compatible".to_owned(),
        "https://api.example.com".to_owned(),
        "model".to_owned(),
        String::new(),
        Duration::from_secs(5),
        poster,
    );
    assert!(matches!(
        processor.process("x"),
        Err(ProcessingError::Response(_))
    ));
}

#[test]
fn none_provider_is_never_built_from_config() {
    let config = voiceless::config::ProcessingConfig::default();
    assert!(voiceless::processing::from_config(&config)
        .expect("build")
        .is_none());
}

#[test]
fn direct_api_key_is_used_as_the_bearer() {
    use voiceless::config::ProcessingConfig;

    let config = ProcessingConfig {
        provider: "openai-compatible".to_owned(),
        api_key: "sk-direct".to_owned(),
        ..Default::default()
    };
    let processor = voiceless::processing::from_config(&config)
        .expect("build")
        .expect("provider configured");

    assert_eq!(processor.api_key, "sk-direct");
    assert_eq!(processor.model, "deepseek-v4-flash-0731");
    assert_eq!(
        processor.base_url,
        "https://dashscope.aliyuncs.com/compatible-mode/v1"
    );
}
