use std::io::{self, BufRead, Read, Write};
use std::path::Path;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::{config::AppConfig, output};

const MAX_SAMPLE_BYTES: u64 = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sample {
    text: String,
    #[serde(default)]
    mode: String,
    app_id: Option<String>,
    site: Option<String>,
    project: Option<String>,
}

async fn evaluate(
    config: &AppConfig,
    sample: &Sample,
    send_model: bool,
) -> Result<output::Preview, String> {
    if sample.text.len() as u64 > MAX_SAMPLE_BYTES {
        return Err("sample must be at most 64 KiB".to_owned());
    }
    let pipeline = output::Pipeline::prepare(
        config,
        &sample.mode,
        "quick",
        output::Context {
            app_id: sample.app_id.as_deref(),
            app_source: Some("preview"),
            site: sample.site.as_deref(),
            site_source: Some("preview"),
            project: sample.project.as_deref(),
        },
    )?;
    Ok(pipeline.run(&sample.text, true, send_model).await)
}

pub struct PreviewArgs {
    pub text: Option<String>,
    pub mode: String,
    pub app_id: Option<String>,
    pub site: Option<String>,
    pub project: Option<String>,
    pub send_model: bool,
    pub json: bool,
}

pub async fn preview(path: Option<&Path>, args: PreviewArgs) -> i32 {
    let result: Result<_, String> = async {
        let text = match args.text {
            Some(text) => text,
            None => {
                let mut text = String::new();
                io::stdin()
                    .take(MAX_SAMPLE_BYTES + 1)
                    .read_to_string(&mut text)
                    .map_err(|error| format!("cannot read the preview sample: {error}"))?;
                text
            }
        };
        let config = AppConfig::load_optional(path).map_err(|error| error.to_string())?;
        evaluate(
            &config,
            &Sample {
                text,
                mode: args.mode,
                app_id: args.app_id,
                site: args.site,
                project: args.project,
            },
            args.send_model,
        )
        .await
    }
    .await;
    match result {
        Ok(preview) => {
            let code = i32::from(preview.error.is_some());
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string(&preview).expect("serializable preview")
                );
            } else {
                if let Some(error) = &preview.error {
                    eprintln!("Voiceless: {error}");
                }
                println!("{}", preview.text);
            }
            code
        }
        Err(error) => {
            if args.json {
                println!("{}", json!({ "error": error }));
            } else {
                eprintln!("Voiceless: {error}");
            }
            1
        }
    }
}

/// Local MCP over stdio for OpenCode. It only sees samples sent by the client.
pub async fn serve_mcp(path: Option<&Path>) -> Result<(), String> {
    allowed_config(path)?;
    let mut input = io::stdin().lock();
    loop {
        let mut line = String::new();
        let bytes = input
            .by_ref()
            .take(MAX_SAMPLE_BYTES * 8)
            .read_line(&mut line)
            .map_err(|error| format!("cannot read MCP request: {error}"))?;
        if bytes == 0 {
            return Ok(());
        }
        if !line.ends_with('\n') && bytes as u64 == MAX_SAMPLE_BYTES * 8 {
            return Err("MCP request exceeds 512 KiB".to_owned());
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(request) => match request.get("id") {
                Some(id) => reply(path, &request, id).await,
                None => continue,
            },
            Err(_) => rpc_error(&Value::Null, -32700, "invalid JSON"),
        };
        let mut stdout = io::stdout().lock();
        serde_json::to_writer(&mut stdout, &response).map_err(|error| error.to_string())?;
        writeln!(stdout)
            .and_then(|_| stdout.flush())
            .map_err(|error| error.to_string())?;
    }
}

fn allowed_config(path: Option<&Path>) -> Result<AppConfig, String> {
    let config = AppConfig::load_optional(path).map_err(|error| error.to_string())?;
    if !config.output.allow_opencode {
        return Err("enable output.allow_opencode to use local OpenCode preview".to_owned());
    }
    Ok(config)
}

async fn reply(path: Option<&Path>, request: &Value, id: &Value) -> Value {
    if request["jsonrpc"] != "2.0"
        || !matches!(id, Value::String(_) | Value::Number(_) | Value::Null)
    {
        return rpc_error(&Value::Null, -32600, "invalid JSON-RPC request");
    }
    let result = match request["method"].as_str() {
        Some("initialize") => {
            let version = request["params"]["protocolVersion"]
                .as_str()
                .unwrap_or_default();
            let version = match version {
                "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25" => version,
                _ => "2025-06-18",
            };
            json!({"protocolVersion": version, "capabilities": {"tools": {}},
                "serverInfo": {"name": "voiceless", "version": env!("CARGO_PKG_VERSION")} })
        }
        Some("ping") => json!({}),
        Some("tools/list") => json!({"tools": [{
            "name": "preview_output", "description": "Run the local dictation output steps on sample text.",
            "inputSchema": {"type": "object", "additionalProperties": false,
                "properties": {"text": {"type": "string"}, "mode": {"type": "string"}, "app_id": {"type": "string"},
                    "site": {"type": "string"}, "project": {"type": "string"}},
                "required": ["text"]}
        }]}),
        Some("tools/call") if request["params"]["name"] == "preview_output" => {
            match serde_json::from_value::<Sample>(request["params"]["arguments"].clone()) {
                Ok(sample) => match allowed_config(path) {
                    Ok(config) => match evaluate(&config, &sample, false).await {
                        Ok(preview) => json!({"isError": preview.error.is_some(),
                            "content": [{"type": "text", "text": serde_json::to_string(&preview).expect("serializable preview")}],
                            "structuredContent": preview}),
                        Err(error) => return rpc_error(id, -32602, &error),
                    },
                    Err(error) => return rpc_error(id, -32000, &error),
                },
                Err(error) => return rpc_error(id, -32602, &error.to_string()),
            }
        }
        Some("tools/call") => return rpc_error(id, -32602, "unknown tool"),
        _ => return rpc_error(id, -32601, "unknown method"),
    };
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}
