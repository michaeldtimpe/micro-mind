//! `bench-compare` — diff a candidate `summary.json` against a baseline
//! `summary.json`. Emit a markdown delta table, JSON deltas, and exit
//! non-zero on outcome / latency / token regressions beyond configurable
//! thresholds.
//!
//! Outcome-regression policy:
//!   - Any task that *passed in baseline* but *failed in candidate* is a
//!     hard regression. Exit 1.
//!   - The reverse is celebrated but doesn't gate.
//!
//! Latency / token policy:
//!   - For tasks that pass in both, p50 wall_ms and total_tokens are
//!     compared. If they regress beyond --wall-pct / --tokens-pct, it's a
//!     soft regression (warn, exit 2 unless --strict).
//!
//! Designed to be runnable in CI on PRs: produce a markdown delta to be
//! posted as a comment and an exit code that gates merge.

use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

use micro_mind::bench::Summary;

#[derive(Parser, Debug)]
#[command(
    name = "bench-compare",
    about = "Compare candidate bench summary against a baseline"
)]
struct Cli {
    /// Baseline summary.json (from a prior bench-run).
    #[arg(long)]
    baseline: PathBuf,
    /// Candidate summary.json (current run).
    #[arg(long)]
    candidate: PathBuf,
    /// Wall_ms regression threshold, %. Default 20.
    #[arg(long, default_value_t = 20)]
    wall_pct: u32,
    /// total_tokens regression threshold, %. Default 10.
    #[arg(long, default_value_t = 10)]
    tokens_pct: u32,
    /// Treat soft regressions (latency/tokens) as hard (exit 1).
    #[arg(long)]
    strict: bool,
    /// Output a markdown delta table to this path.
    #[arg(long)]
    md: Option<PathBuf>,
    /// Output a JSON delta to this path.
    #[arg(long)]
    json: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
struct LoadedSummary {
    #[allow(dead_code)]
    schema_v: Option<u32>,
    outcomes: Vec<LoadedOutcome>,
}

#[derive(Debug, Clone, Deserialize)]
struct LoadedOutcome {
    id: String,
    passed: bool,
    #[serde(default)]
    #[allow(dead_code)]
    // kept so the bench-run schema round-trips; printed by callers in the future
    failures: Vec<String>,
    stats: Summary,
}

#[derive(Debug, Clone, Serialize)]
struct Delta {
    id: String,
    baseline_passed: Option<bool>,
    candidate_passed: Option<bool>,
    baseline_wall_ms: Option<u64>,
    candidate_wall_ms: Option<u64>,
    wall_pct_delta: Option<f64>,
    baseline_tokens: Option<u64>,
    candidate_tokens: Option<u64>,
    tokens_pct_delta: Option<f64>,
    classification: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let baseline = load(&cli.baseline)?;
    let candidate = load(&cli.candidate)?;

    let bmap: BTreeMap<&str, &LoadedOutcome> = baseline
        .outcomes
        .iter()
        .map(|o| (o.id.as_str(), o))
        .collect();
    let cmap: BTreeMap<&str, &LoadedOutcome> = candidate
        .outcomes
        .iter()
        .map(|o| (o.id.as_str(), o))
        .collect();

    let mut ids: Vec<&str> = bmap.keys().chain(cmap.keys()).copied().collect();
    ids.sort();
    ids.dedup();

    let mut deltas = Vec::new();
    let mut hard_regressions = 0u32;
    let mut soft_regressions = 0u32;
    let mut improvements = 0u32;

    for id in ids {
        let b = bmap.get(id).copied();
        let c = cmap.get(id).copied();
        let baseline_passed = b.map(|o| o.passed);
        let candidate_passed = c.map(|o| o.passed);
        let baseline_wall = b.map(|o| o.stats.wall_ms);
        let candidate_wall = c.map(|o| o.stats.wall_ms);
        let baseline_tokens = b.map(|o| o.stats.total_tokens);
        let candidate_tokens = c.map(|o| o.stats.total_tokens);

        let wall_pct_delta = pct_delta(baseline_wall, candidate_wall);
        let tokens_pct_delta = pct_delta(baseline_tokens, candidate_tokens);

        let classification = match (baseline_passed, candidate_passed) {
            (Some(true), Some(false)) => {
                hard_regressions += 1;
                "OUTCOME-REGRESSION".into()
            }
            (Some(false), Some(true)) => {
                improvements += 1;
                "outcome-improvement".into()
            }
            (Some(_), None) => "missing-in-candidate".into(),
            (None, Some(_)) => "new-in-candidate".into(),
            (Some(true), Some(true)) => {
                let wall_bad = wall_pct_delta
                    .map(|p| p > cli.wall_pct as f64)
                    .unwrap_or(false);
                let tok_bad = tokens_pct_delta
                    .map(|p| p > cli.tokens_pct as f64)
                    .unwrap_or(false);
                if wall_bad || tok_bad {
                    soft_regressions += 1;
                    let mut tags = Vec::new();
                    if wall_bad {
                        tags.push("wall");
                    }
                    if tok_bad {
                        tags.push("tokens");
                    }
                    format!("soft-regression ({})", tags.join(","))
                } else {
                    "ok".into()
                }
            }
            (Some(false), Some(false)) => "both-failing".into(),
            (None, None) => "missing-both".into(),
        };

        deltas.push(Delta {
            id: id.to_string(),
            baseline_passed,
            candidate_passed,
            baseline_wall_ms: baseline_wall,
            candidate_wall_ms: candidate_wall,
            wall_pct_delta,
            baseline_tokens,
            candidate_tokens,
            tokens_pct_delta,
            classification,
        });
    }

    print_table(&deltas);

    if let Some(p) = &cli.md {
        std::fs::write(p, render_markdown(&deltas))
            .with_context(|| format!("write md {}", p.display()))?;
        eprintln!("wrote {}", p.display());
    }
    if let Some(p) = &cli.json {
        std::fs::write(p, serde_json::to_string_pretty(&deltas)?)
            .with_context(|| format!("write json {}", p.display()))?;
        eprintln!("wrote {}", p.display());
    }

    println!(
        "\n{} hard regressions, {} soft regressions, {} improvements",
        hard_regressions, soft_regressions, improvements
    );

    if hard_regressions > 0 {
        std::process::exit(1);
    }
    if soft_regressions > 0 && cli.strict {
        std::process::exit(1);
    }
    if soft_regressions > 0 {
        std::process::exit(2);
    }
    Ok(())
}

fn load(path: &PathBuf) -> Result<LoadedSummary> {
    let body = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let s: LoadedSummary =
        serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
    Ok(s)
}

fn pct_delta(b: Option<u64>, c: Option<u64>) -> Option<f64> {
    match (b, c) {
        (Some(b), Some(c)) if b > 0 => Some(((c as f64 - b as f64) / b as f64) * 100.0),
        _ => None,
    }
}

fn print_table(deltas: &[Delta]) {
    println!(
        "{:<32} {:>16} {:>16} {:>16}",
        "task", "wall_ms (Δ%)", "tokens (Δ%)", "class"
    );
    for d in deltas {
        let wall = match (d.candidate_wall_ms, d.wall_pct_delta) {
            (Some(w), Some(p)) => format!("{w} ({:+.1}%)", p),
            (Some(w), None) => format!("{w}"),
            (None, _) => "-".into(),
        };
        let tok = match (d.candidate_tokens, d.tokens_pct_delta) {
            (Some(t), Some(p)) => format!("{t} ({:+.1}%)", p),
            (Some(t), None) => format!("{t}"),
            (None, _) => "-".into(),
        };
        println!(
            "{:<32} {:>16} {:>16} {:>16}",
            d.id, wall, tok, d.classification
        );
    }
}

fn render_markdown(deltas: &[Delta]) -> String {
    let mut s = String::new();
    s.push_str("| task | baseline pass | candidate pass | wall_ms Δ% | tokens Δ% | class |\n");
    s.push_str("|---|---|---|---:|---:|---|\n");
    for d in deltas {
        s.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} |\n",
            d.id,
            d.baseline_passed
                .map(|b| if b { "✓" } else { "✗" })
                .unwrap_or("—"),
            d.candidate_passed
                .map(|b| if b { "✓" } else { "✗" })
                .unwrap_or("—"),
            d.wall_pct_delta
                .map(|p| format!("{:+.1}", p))
                .unwrap_or_else(|| "—".into()),
            d.tokens_pct_delta
                .map(|p| format!("{:+.1}", p))
                .unwrap_or_else(|| "—".into()),
            d.classification,
        ));
    }
    s
}
