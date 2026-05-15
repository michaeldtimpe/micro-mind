//! `bench-summarize` — read a directory of JSONL traces (or a single file)
//! and emit a machine-readable summary plus an optional markdown table.
//!
//! Usable on traces produced by either `bench-run` or by a plain
//! `micro-mind --record ...` REPL session.

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

use micro_mind::bench::{Summary, parse_jsonl_file, summarize_trace};

#[derive(Parser, Debug)]
#[command(
    name = "bench-summarize",
    about = "Aggregate one or more JSONL traces into a summary"
)]
struct Cli {
    /// One or more JSONL files, or a directory containing them.
    paths: Vec<PathBuf>,

    /// Print a markdown table to stdout.
    #[arg(long)]
    md: bool,

    /// Write JSON summary to this path.
    #[arg(long)]
    json: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.paths.is_empty() {
        anyhow::bail!("at least one path is required");
    }

    let mut rows: Vec<(String, Summary)> = Vec::new();
    for input in &cli.paths {
        if input.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(input)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
                .collect();
            entries.sort();
            for p in entries {
                summarize_one(&p, &mut rows)?;
            }
        } else {
            summarize_one(input, &mut rows)?;
        }
    }

    if rows.is_empty() {
        anyhow::bail!("no JSONL files found");
    }

    if cli.md {
        print_markdown(&rows);
    } else {
        // Default: short text table on stdout.
        print_text(&rows);
    }

    if let Some(out) = &cli.json {
        let payload = serde_json::json!({
            "schema_v": 1,
            "rows": rows.iter().map(|(p, s)| serde_json::json!({"path": p, "summary": s})).collect::<Vec<_>>(),
        });
        std::fs::write(out, serde_json::to_string_pretty(&payload)?)
            .with_context(|| format!("write {}", out.display()))?;
        eprintln!("wrote {}", out.display());
    }

    Ok(())
}

fn summarize_one(path: &PathBuf, rows: &mut Vec<(String, Summary)>) -> Result<()> {
    let events = parse_jsonl_file(path)?;
    let s = summarize_trace(&events);
    rows.push((path.display().to_string(), s));
    Ok(())
}

fn print_text(rows: &[(String, Summary)]) {
    println!(
        "{:<40} {:>6} {:>8} {:>7} {:>5} {:>6} {:>5} stop",
        "trace", "tools", "tokens", "wall_ms", "guard", "nat", "rec"
    );
    for (path, s) in rows {
        let path_short = shorten(path, 40);
        println!(
            "{:<40} {:>6} {:>8} {:>7} {:>5} {:>6} {:>5} {}",
            path_short,
            s.tool_calls,
            s.total_tokens,
            s.wall_ms,
            s.guard_fires,
            s.native_tool_calls,
            s.recovered_tool_calls,
            s.stop_reason.as_deref().unwrap_or("?")
        );
    }
}

fn print_markdown(rows: &[(String, Summary)]) {
    println!("| trace | tools | tokens | wall_ms | guards | native | recovered | stop |");
    println!("|---|---:|---:|---:|---:|---:|---:|---|");
    for (path, s) in rows {
        println!(
            "| `{}` | {} | {} | {} | {} | {} | {} | {} |",
            shorten(path, 60),
            s.tool_calls,
            s.total_tokens,
            s.wall_ms,
            s.guard_fires,
            s.native_tool_calls,
            s.recovered_tool_calls,
            s.stop_reason.as_deref().unwrap_or("?")
        );
    }
}

fn shorten(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let tail = &s[s.len() - (max - 1)..];
        format!("…{tail}")
    }
}
