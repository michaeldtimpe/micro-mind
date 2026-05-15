//! Singleton llama-server lifecycle.
//!
//! Two modes:
//!   1. If LLAMA_SERVER_URL is set in the environment, attach to it. The
//!      user owns the server; we never spawn/kill.
//!   2. Otherwise, spawn `llama-server` once at process start using the
//!      neo-llm-bench champion config. SIGTERM only on process exit
//!      (Ctrl-D / `/quit`). The server is NOT restarted on `/reset`.

use anyhow::{Context, Result};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config;

const DEFAULT_LLAMA_SERVER_PATH: &str = "/Users/mtimpe/code/llama.cpp/build/bin/llama-server";
const DEFAULT_PORT: u16 = 8080;

pub struct ServerHandle {
    pub url: String,
    /// None if we attached to an externally-managed server.
    child: Option<Child>,
}

impl ServerHandle {
    pub fn attach_or_spawn() -> Result<Self> {
        // 1. Honor env override.
        if let Ok(url) = std::env::var("LLAMA_SERVER_URL") {
            let url = url.trim().trim_end_matches('/').to_string();
            wait_for_health(&url, Duration::from_secs(5))
                .with_context(|| format!("LLAMA_SERVER_URL={url} is not responding to /health"))?;
            return Ok(Self { url, child: None });
        }

        // 2. Try the default port first — maybe llama-server is already running.
        let default_url = format!("http://127.0.0.1:{}", DEFAULT_PORT);
        if wait_for_health(&default_url, Duration::from_secs(1)).is_ok() {
            return Ok(Self { url: default_url, child: None });
        }

        // 3. Spawn. Resolve binary in priority order:
        //    a. MICROMIND_LLAMA_SERVER env var (explicit override).
        //    b. `llama-server` on PATH.
        //    c. DEFAULT_LLAMA_SERVER_PATH (developer machine fallback).
        let bin = resolve_llama_server_bin();
        let model_path = std::env::var("MICROMIND_MODEL_PATH")
            .ok()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| config::expand_tilde(config::DEFAULT_MODEL_PATH));

        if !std::path::Path::new(&bin).exists() {
            anyhow::bail!(
                "llama-server not found at {bin}. \
                 Set MICROMIND_LLAMA_SERVER, put llama-server on PATH, \
                 or run llama-server yourself and set LLAMA_SERVER_URL."
            );
        }
        if !model_path.exists() {
            anyhow::bail!(
                "Model file not found at {}. Set MICROMIND_MODEL_PATH to override.",
                model_path.display()
            );
        }

        let child = Command::new(&bin)
            .args(&[
                "-m", &model_path.to_string_lossy(),
                "--ctx-size", &config::N_CTX.to_string(),
                "--n-gpu-layers", &config::N_GPU_LAYERS.to_string(),
                "--threads", &config::N_THREADS.to_string(),
                "--batch-size", &config::N_BATCH.to_string(),
                "--ubatch-size", &config::N_UBATCH.to_string(),
                "--cache-type-k", "q8_0",
                "--cache-type-v", "q8_0",
                "--jinja",
                "--port", &DEFAULT_PORT.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())   // suppress noisy startup output
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn llama-server: {bin}"))?;

        let url = format!("http://127.0.0.1:{}", DEFAULT_PORT);
        eprintln!("spawning llama-server (pid {}), waiting for /health …", child.id());
        wait_for_health(&url, Duration::from_secs(60))
            .context("llama-server did not become healthy within 60s")?;

        Ok(Self { url, child: Some(child) })
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            #[cfg(unix)]
            {
                use nix::sys::signal::{Signal, kill};
                use nix::unistd::Pid;
                let _ = kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM);
            }
            // Give it a moment to exit gracefully, then force.
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(5) {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Resolve the `llama-server` binary, in priority order:
///   1. `MICROMIND_LLAMA_SERVER` env var (explicit override — used verbatim).
///   2. `llama-server` on `PATH` (preferred for CI / packaged installs).
///   3. `DEFAULT_LLAMA_SERVER_PATH` (developer machine fallback).
///
/// Returns the candidate path as a `String`. Existence is checked by the caller
/// so we can produce a single, actionable error message.
fn resolve_llama_server_bin() -> String {
    if let Ok(v) = std::env::var("MICROMIND_LLAMA_SERVER") {
        let v = v.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    if let Some(found) = find_on_path("llama-server") {
        return found;
    }
    DEFAULT_LLAMA_SERVER_PATH.to_string()
}

/// Minimal PATH search — no extra dependency. Returns the first match that exists
/// as a regular file in any PATH directory.
fn find_on_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_prefers_env_var() {
        // SAFETY: tests run single-threaded by default per env-mutating tests; the value
        // here is restored at the end. If multiple env-tests are added, gate with a mutex.
        let prev = std::env::var("MICROMIND_LLAMA_SERVER").ok();
        unsafe { std::env::set_var("MICROMIND_LLAMA_SERVER", "/explicit/override"); }
        assert_eq!(resolve_llama_server_bin(), "/explicit/override");
        match prev {
            Some(v) => unsafe { std::env::set_var("MICROMIND_LLAMA_SERVER", v) },
            None => unsafe { std::env::remove_var("MICROMIND_LLAMA_SERVER") },
        }
    }

    #[test]
    fn find_on_path_locates_common_binary() {
        // `sh` is on PATH in every Unix CI env we ship to.
        let r = find_on_path("sh");
        assert!(r.is_some(), "expected to find sh on PATH");
    }

    #[test]
    fn find_on_path_returns_none_for_garbage() {
        assert!(find_on_path("definitely-not-a-real-binary-xyzzy").is_none());
    }
}

fn wait_for_health(url: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let health_url = format!("{}/health", url.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(500))
        .build();
    loop {
        match agent.get(&health_url).call() {
            Ok(resp) if resp.status() < 500 => return Ok(()),
            _ => {}
        }
        if Instant::now() >= deadline {
            anyhow::bail!("/health did not respond at {}", health_url);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
