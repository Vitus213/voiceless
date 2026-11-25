//! The script language receives one string and returns one string. Its
//! interpreter has no host capabilities, and the caller owns its deadline.

use std::io::{Read, Write};
use std::process::Stdio;
use std::time::Duration;

use rhai::{Engine, ImmutableString, Scope};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

#[doc(hidden)]
pub const SCRIPT_WORKER_ARG: &str = "__output-script-worker";

const MAX_SOURCE: usize = 16 * 1024;
const MAX_TEXT: usize = 64 * 1024;
const MAX_REQUEST: u64 = (6 * (MAX_SOURCE + MAX_TEXT) + 256) as u64;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Script {
    pub source: String,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub allowed: bool,
}

fn default_timeout() -> u64 {
    500
}

impl Script {
    pub(super) fn validate(&self) -> Result<(), String> {
        if self.source.len() > MAX_SOURCE {
            return Err("output script source exceeds 16 KiB".to_owned());
        }
        if !(1..=5_000).contains(&self.timeout_ms) {
            return Err("output script timeout must be 1–5000 ms".to_owned());
        }
        Ok(())
    }

    pub(super) async fn run(&self, text: &str) -> Result<String, String> {
        if !self.allowed {
            return Err("this mode needs permission to run its output script".to_owned());
        }
        if self.source.trim().is_empty() {
            return Err("this mode has no output script source".to_owned());
        }
        if text.len() > MAX_TEXT {
            return Err("output script input exceeds 64 KiB".to_owned());
        }
        let request = serde_json::to_vec(&Invocation {
            source: self.source.as_str(),
            text,
        })
        .map_err(|error| error.to_string())?;
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let mut command = tokio::process::Command::new(executable);
        command
            .arg(SCRIPT_WORKER_ARG)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // Standalone and Nix launchers can supply their native library path.
        if let Some(libraries) = std::env::var_os("LD_LIBRARY_PATH") {
            command.env("LD_LIBRARY_PATH", libraries);
        }
        let deadline = Duration::from_millis(self.timeout_ms);
        let output = tokio::time::timeout(deadline, async {
            let mut child = command.spawn()?;
            let mut input = child.stdin.take().expect("piped script input");
            input.write_all(&request).await?;
            drop(input);
            child.wait_with_output().await
        })
        .await
        .map_err(|_| format!("output script exceeded {} ms", self.timeout_ms))?
        .map_err(|error| format!("cannot run output script worker: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "output script worker exited with {}",
                output.status
            ));
        }
        serde_json::from_slice::<Result<String, String>>(&output.stdout)
            .map_err(|error| format!("invalid output script response: {error}"))?
    }
}

#[derive(Deserialize, Serialize)]
struct Invocation<T> {
    source: T,
    text: T,
}

#[doc(hidden)]
pub fn run_script_worker() -> Result<(), String> {
    let mut request = Vec::new();
    std::io::stdin()
        .take(MAX_REQUEST + 1)
        .read_to_end(&mut request)
        .map_err(|error| error.to_string())?;
    if request.len() as u64 > MAX_REQUEST {
        return Err("output script request exceeds its size limit".to_owned());
    }
    let invocation: Invocation<String> =
        serde_json::from_slice(&request).map_err(|error| error.to_string())?;
    drop(request);
    let result = evaluate(invocation);
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, &result).map_err(|error| error.to_string())?;
    stdout.flush().map_err(|error| error.to_string())
}

fn evaluate(Invocation { source, text }: Invocation<String>) -> Result<String, String> {
    if source.len() > MAX_SOURCE || text.len() > MAX_TEXT {
        return Err("output script source or input exceeds its size limit".to_owned());
    }
    // Module loading and clocks are disabled at build time. No native I/O
    // functions or application objects are registered with this engine.
    let mut engine = Engine::new();
    engine
        .on_print(|_| {})
        .on_debug(|_, _, _| {})
        .set_max_operations(5_000_000)
        .set_max_call_levels(24)
        .set_max_expr_depths(32, 32)
        .set_max_variables(64)
        .set_max_functions(16)
        .set_max_string_size(MAX_TEXT)
        .set_max_array_size(256)
        .set_max_map_size(64);
    let output = engine
        .eval_with_scope::<ImmutableString>(Scope::new().push("text", text), &source)
        .map_err(|error| {
            let detail: String = error.to_string().chars().take(512).collect();
            format!("output script failed: {detail}")
        })?;
    if output.len() > MAX_TEXT {
        return Err("output script result exceeds 64 KiB".to_owned());
    }
    Ok(output.to_string())
}
