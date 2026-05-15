//! `bench-replay` — validate a JSONL trace against a fixture without
//! running the model. CI-friendly: no llama-server, no GPU.
//!
//! Usage:
//!   bench-replay --fixture bench/tasks/03.toml --trace bench/runs/<ts>/03-rep0.jsonl
//!   bench-replay --all bench/tasks --runs bench/runs/<ts>
//!
//! Exits non-zero on the first failure (or aggregated failures with --all).
//! Same predicate set as bench-run, just driven from disk.

use anyhow::{Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};

use micro_mind::bench::summary::check_expectations;
use micro_mind::bench::{Fixture, parse_jsonl_file, summarize_trace};

#[derive(Parser, Debug)]
#[command(
    name = "bench-replay",
    about = "Validate a JSONL trace against a bench fixture"
)]
struct Cli {
    /// Single-trace mode: path to the fixture TOML.
    #[arg(long)]
    fixture: Option<PathBuf>,
    /// Single-trace mode: path to the JSONL trace.
    #[arg(long)]
    trace: Option<PathBuf>,
    /// Batch mode: directory of fixture TOMLs.
    #[arg(long)]
    all: Option<PathBuf>,
    /// Batch mode: directory of JSONL traces (must contain `<task-id>-repN.jsonl`).
    #[arg(long)]
    runs: Option<PathBuf>,
    /// Schema-only mode: just check that each line parses as a known event.
    #[arg(long)]
    schema_only: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match (
        &cli.fixture,
        &cli.trace,
        &cli.all,
        &cli.runs,
        cli.schema_only,
    ) {
        (Some(fx), Some(tr), None, None, _) => run_single(fx, tr),
        (None, None, Some(tasks), Some(runs), _) => run_batch(tasks, runs),
        (None, Some(tr), None, None, true) => run_schema_only(tr),
        _ => {
            anyhow::bail!(
                "usage: --fixture F --trace T  |  --all DIR --runs DIR  |  --trace T --schema-only"
            )
        }
    }
}

fn run_single(fx_path: &Path, trace_path: &Path) -> Result<()> {
    let fx = Fixture::from_path(fx_path)?;
    let events = parse_jsonl_file(trace_path)?;
    let stats = summarize_trace(&events);
    let fails = check_expectations(&fx, &stats);
    if fails.is_empty() {
        println!(
            "ok  {}  (tools={} wall_ms={} tokens={})",
            fx.id, stats.tool_calls, stats.wall_ms, stats.total_tokens
        );
        Ok(())
    } else {
        println!("FAIL {}: {}", fx.id, fails.join("; "));
        std::process::exit(1);
    }
}

fn run_batch(tasks_dir: &Path, runs_dir: &Path) -> Result<()> {
    let fixtures = Fixture::discover(tasks_dir)
        .with_context(|| format!("discover fixtures in {}", tasks_dir.display()))?;
    let mut failures = 0u32;
    let mut checked = 0u32;
    let mut missing = 0u32;
    for fx in &fixtures {
        // Find traces matching <id>-rep*.jsonl
        let pattern_prefix = format!("{}-rep", fx.id);
        let mut found_any = false;
        let entries = match std::fs::read_dir(runs_dir) {
            Ok(e) => e,
            Err(e) => {
                anyhow::bail!("read runs dir {}: {e}", runs_dir.display());
            }
        };
        for e in entries {
            let p = e?.path();
            let name = match p.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if name.starts_with(&pattern_prefix) && name.ends_with(".jsonl") {
                found_any = true;
                checked += 1;
                let events = parse_jsonl_file(&p)?;
                let stats = summarize_trace(&events);
                let fails = check_expectations(fx, &stats);
                if fails.is_empty() {
                    println!("ok   {}  {}", fx.id, name);
                } else {
                    failures += 1;
                    println!("FAIL {}  {}: {}", fx.id, name, fails.join("; "));
                }
            }
        }
        if !found_any {
            missing += 1;
            eprintln!("(no trace found for fixture {})", fx.id);
        }
    }
    println!("\n{checked} checked, {failures} failures, {missing} missing");
    if failures > 0 || (checked == 0 && missing > 0) {
        std::process::exit(1);
    }
    Ok(())
}

fn run_schema_only(trace_path: &Path) -> Result<()> {
    let events = parse_jsonl_file(trace_path)?;
    println!(
        "ok  {} events parsed from {}",
        events.len(),
        trace_path.display()
    );
    Ok(())
}
