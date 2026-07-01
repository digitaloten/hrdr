//! TEMPORARY model-backend bootstrap.
//!
//! For now hrdr spawns a local `llama-server` (llama.cpp) as its
//! OpenAI-compatible backend, so the harness can be refined against a real
//! tool-calling model. **This is a stopgap.**
//!
//! REMOVE / REPLACE once `infr`'s serve path supports agentic tool use — today
//! `infr`'s `Engine::chat` is a `todo!` and its `LlamaGenerator` ignores
//! `_tools_json` and drops everything but the last user message, so the model
//! never sees the tools. When infr wires up full-history chat-template
//! rendering + tool-def injection + `<|tool_call>` parsing, delete this module
//! and point hrdr straight at `infr serve` (`--no-backend`, or set
//! `HRDR_BASE_URL`). hrdr itself needs no change — only this bootstrap goes
//! away.

use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hrdr_llm::Client;
use tokio::process::{Child, Command};

/// How a managed backend was provisioned.
pub enum Backend {
    /// We launched `llama-server`; it is killed when this value drops (the
    /// held `Child` is a kill-on-drop RAII guard, never read directly). Boxed so
    /// the enum stays small (the `Child` is larger on Windows).
    Spawned(#[allow(dead_code)] Box<Child>),
    /// A backend was already reachable; we reuse it and own nothing.
    External,
}

/// Settings for the spawned `llama-server`.
#[derive(Clone)]
pub struct BackendConfig {
    /// Model ref (HF `org/repo:quant`) or path to a `.gguf`.
    pub model: String,
    /// `llama-server` binary name or path.
    pub bin: String,
    /// Context window size.
    pub ctx: u32,
    /// Extra args passed through verbatim (e.g. `-ngl 99` for GPU offload).
    pub extra_args: Vec<String>,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            model: "unsloth/Qwen3-30B-A3B-GGUF:Q4_K_M".to_string(),
            bin: "llama-server".to_string(),
            ctx: 16384,
            extra_args: Vec::new(),
        }
    }
}

impl Backend {
    /// Ensure a backend answers at `base_url`. Reuse one if already up,
    /// otherwise spawn `llama-server --jinja` and block until it is ready.
    pub async fn ensure(cfg: &BackendConfig, base_url: &str) -> Result<Self> {
        let probe = Client::new(base_url, None, "default");
        if probe.list_models().await.is_ok() {
            eprintln!("hrdr: reusing existing backend at {base_url}");
            return Ok(Backend::External);
        }

        let (host, port) = parse_host_port(base_url)?;
        let log_path = log_file();
        eprintln!(
            "hrdr: starting llama-server ({}) on {host}:{port} — loading model, this can take a minute…\n      logs: {}",
            cfg.model,
            log_path.display(),
        );

        let log = std::fs::File::create(&log_path)
            .with_context(|| format!("creating {}", log_path.display()))?;
        let log_err = log.try_clone()?;

        // `--jinja` is REQUIRED: it enables the chat template that injects the
        // tool definitions and parses the model's tool calls back into the
        // OpenAI shape. Without it the model never sees the tools.
        let child = Command::new(&cfg.bin)
            .arg("-hf")
            .arg(&cfg.model)
            .arg("--jinja")
            .arg("-c")
            .arg(cfg.ctx.to_string())
            .arg("--host")
            .arg(&host)
            .arg("--port")
            .arg(port.to_string())
            .args(&cfg.extra_args)
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "spawning `{}` — is llama.cpp installed? (use --no-backend to skip and point \
                     --base-url at your own endpoint)",
                    cfg.bin
                )
            })?;

        if !wait_ready(&probe, Duration::from_secs(300)).await {
            bail!(
                "llama-server did not become ready within 5 min — see {}",
                log_path.display()
            );
        }
        eprintln!("hrdr: backend ready.");
        Ok(Backend::Spawned(Box::new(child)))
    }
}

async fn wait_ready(client: &Client, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if client.list_models().await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

/// Extract `(host, port)` from a base URL like `http://localhost:8080/v1`.
/// `localhost` is normalised to `127.0.0.1` for the server bind address.
fn parse_host_port(base_url: &str) -> Result<(String, u16)> {
    let after = base_url.split("://").nth(1).unwrap_or(base_url);
    let authority = after.split('/').next().unwrap_or(after);
    let (host, port) = authority
        .split_once(':')
        .context("base_url must include host:port to spawn a backend")?;
    let host = if host == "localhost" {
        "127.0.0.1"
    } else {
        host
    };
    let port: u16 = port.parse().context("invalid port in base_url")?;
    Ok((host.to_string(), port))
}

fn log_file() -> std::path::PathBuf {
    let dir = std::env::var("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".cache")
        })
        .join("hrdr");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("llama-server.log")
}
