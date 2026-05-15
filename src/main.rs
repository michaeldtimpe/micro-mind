mod agent;
mod config;
mod llm;
mod repl;
mod server;
mod tools;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;

use micro_mind::obs::{Event, JsonlRecorder, NoopRecorder, Recorder as _, RecorderHandle};

use crate::agent::Session;
use crate::llm::client::LlmClient;
use crate::llm::prompt::system_prompt;
use crate::server::ServerHandle;
use crate::tools::ToolDef;

#[derive(Parser, Debug)]
#[command(
    name = "micro-mind",
    about = "Claude Code clone powered by qwen25-1.5b-instruct via llama-server"
)]
struct Cli {
    /// Working directory the model operates in. Defaults to the current dir.
    #[arg(short = 'C', long)]
    cwd: Option<PathBuf>,

    /// Don't spawn a server even if LLAMA_SERVER_URL is unset (will fail if no server is running).
    #[arg(long)]
    no_spawn: bool,

    /// Append observability events to `<dir>/micro-mind-<ms>.jsonl`. See `obs/schema.md`.
    #[arg(long, value_name = "DIR")]
    record: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = match cli.cwd {
        Some(p) => p
            .canonicalize()
            .with_context(|| format!("could not resolve --cwd {}", p.display()))?,
        None => std::env::current_dir().context("current_dir failed")?,
    };

    if cli.no_spawn && std::env::var("LLAMA_SERVER_URL").is_err() {
        anyhow::bail!("--no-spawn requires LLAMA_SERVER_URL to be set");
    }
    let server = ServerHandle::attach_or_spawn()?;

    let client = LlmClient::new(server.url());
    let tools = build_tool_surface(&cwd);
    let prompt = system_prompt(&cwd);

    let recorder: RecorderHandle = match &cli.record {
        Some(dir) => match JsonlRecorder::open_in_dir(dir) {
            Ok(r) => {
                eprintln!("recording to {}", r.path.display());
                let handle: RecorderHandle = Arc::new(r);
                handle.record(Event::SessionStart {
                    cwd: cwd.display().to_string(),
                    model: client.model_name.clone(),
                    tools: tools.iter().map(|t| t.name.clone()).collect(),
                });
                handle
            }
            Err(e) => {
                eprintln!("(--record disabled: {e})");
                Arc::new(NoopRecorder)
            }
        },
        None => Arc::new(NoopRecorder),
    };

    let session = Session::new(client, tools, cwd, prompt, recorder);

    repl::run(session)?;
    // server is dropped here → SIGTERM if we own it.
    drop(server);
    Ok(())
}

fn build_tool_surface(cwd: &PathBuf) -> Vec<ToolDef> {
    use crate::tools::{fs_read, fs_write, shell};
    vec![
        fs_read::read_file(cwd.clone()),
        fs_read::list_dir(cwd.clone()),
        fs_read::list_files_recursive(cwd.clone()),
        fs_read::grep(cwd.clone()),
        fs_write::write_file(cwd.clone()),
        fs_write::edit_file(cwd.clone()),
        shell::bash(cwd.clone()),
    ]
}
