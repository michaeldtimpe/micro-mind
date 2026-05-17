//! `bench-replay` — validate a JSONL trace against a fixture without
//! running the model. CI-friendly: no llama-server, no GPU.
//!
//! Usage:
//!   bench-replay --fixture bench/tasks/03.toml --trace bench/runs/<ts>/03-rep0.jsonl
//!   bench-replay --all bench/tasks --runs bench/runs/<ts>
//!   bench-replay --schema-only --trace bench/samples/sample-trace.jsonl
//!   bench-replay --migration-check bench/baselines/archive
//!
//! Exits non-zero on the first failure (or aggregated failures with --all).
//! Same predicate set as bench-run, just driven from disk.

use anyhow::{Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};

use micro_mind::bench::summary::{Summary, check_expectations};
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
    /// Migration-check mode: recursively walk `<DIR>` for `*.jsonl` traces,
    /// parse each, run `summarize_trace`, and assert the minimal
    /// compatibility surface holds (see `obs/schema.md` v3 "Replay
    /// invariants" + `bench/PREDICATES.md`'s schema-migration contract).
    ///
    /// Hermetic — no fixture matching, no predicate evaluation, no model.
    /// Pins the *deserialization* contract: any v1/v2/v3 trace parses and
    /// produces a `Summary` where the model/synthetic tool_call accounting
    /// is internally consistent. Pinned fields: `tool_calls`, `tool_errors`,
    /// `guards_by_kind`, `synthetic_tool_calls`, `synthetic_tool_calls_by_name`,
    /// `model_tool_calls`. Not pinned: `total_tokens`, `wall_ms`,
    /// `final_answer` (drift expected across model / server-state).
    #[arg(long)]
    migration_check: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match (
        &cli.fixture,
        &cli.trace,
        &cli.all,
        &cli.runs,
        cli.schema_only,
        &cli.migration_check,
    ) {
        (Some(fx), Some(tr), None, None, _, None) => run_single(fx, tr),
        (None, None, Some(tasks), Some(runs), _, None) => run_batch(tasks, runs),
        (None, Some(tr), None, None, true, None) => run_schema_only(tr),
        (None, None, None, None, false, Some(dir)) => run_migration_check(dir),
        _ => {
            anyhow::bail!(
                "usage: --fixture F --trace T  |  --all DIR --runs DIR  |  \
                 --trace T --schema-only  |  --migration-check DIR"
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

/// Recursively collect every `*.jsonl` under `root`. Order is the
/// directory-tree natural order from `std::fs::read_dir`; sorted within
/// each directory level for stable CI output across runs.
fn collect_jsonl(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !root.exists() {
        anyhow::bail!("migration-check path does not exist: {}", root.display());
    }
    if !root.is_dir() {
        // Single-file mode: useful for ad-hoc invocation against one trace.
        if root.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.push(root.to_path_buf());
            return Ok(out);
        }
        anyhow::bail!(
            "migration-check expects a directory or a .jsonl file: {}",
            root.display()
        );
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(root)
        .with_context(|| format!("read dir {}", root.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            let mut nested = collect_jsonl(&path)?;
            out.append(&mut nested);
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
    Ok(out)
}

/// Assert the minimal compatibility surface pinned by the schema-migration
/// contract (see `obs/schema.md` v3, `bench/PREDICATES.md` schema-migration
/// section). Today the only structural invariant we can assert at the
/// `Summary` level — beyond "all fields parsed cleanly" which is implied
/// by reaching this function — is the accounting identity between model
/// and synthetic tool calls.
///
/// If a future schema change reorganizes provenance (e.g., a third origin
/// variant), the identity must be preserved or the migration contract
/// breaks; this function is where to update the assertion.
fn check_minimal_surface(summary: &Summary) -> Result<()> {
    let model = summary.model_tool_calls;
    let synth = summary.synthetic_tool_calls;
    let total = summary.tool_calls;
    if model + synth != total {
        anyhow::bail!(
            "tool_calls accounting violates the migration invariant: \
             model_tool_calls ({model}) + synthetic_tool_calls ({synth}) != \
             tool_calls ({total}). If `summarize_trace` was changed, ensure \
             the derived `model_tool_calls` field stays consistent."
        );
    }
    Ok(())
}

/// Walk a directory tree for `*.jsonl` traces; parse each and assert the
/// schema-migration contract holds. Aggregates failures and exits non-zero
/// on any failure (parse error, accounting inconsistency, or empty tree).
fn run_migration_check(root: &Path) -> Result<()> {
    let traces = collect_jsonl(root)?;
    if traces.is_empty() {
        anyhow::bail!(
            "no `*.jsonl` traces found under {} — migration check needs \
             at least one trace to validate",
            root.display()
        );
    }
    let mut checked = 0u32;
    let mut failures = 0u32;
    for trace in &traces {
        match parse_jsonl_file(trace) {
            Err(e) => {
                failures += 1;
                println!("PARSE  {}: {e}", trace.display());
                continue;
            }
            Ok(events) => {
                let summary = summarize_trace(&events);
                if let Err(e) = check_minimal_surface(&summary) {
                    failures += 1;
                    println!("SURFACE  {}: {e}", trace.display());
                    continue;
                }
                checked += 1;
                println!(
                    "ok   {}  ({} events; total={} model={} synth={})",
                    trace.display(),
                    events.len(),
                    summary.tool_calls,
                    summary.model_tool_calls,
                    summary.synthetic_tool_calls
                );
            }
        }
    }
    println!(
        "\n{checked} traces parsed cleanly, {failures} failures (over {} files)",
        traces.len()
    );
    if failures > 0 {
        std::process::exit(1);
    }
    Ok(())
}
