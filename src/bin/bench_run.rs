//! `bench-run` — drive micro-mind on each fixture, write JSONL traces.
//!
//! Strategy: spawn the `micro-mind` binary as a subprocess with --record,
//! pipe the fixture's prompt on stdin, then send `/quit` to end the REPL.
//! Each repetition gets its own trace file. Pass/fail check runs after.
//!
//! Why subprocess and not in-process? The Session is fundamentally tied to
//! a live llama-server, and embedding pulls all of agent/tools/repl into
//! this binary's dependency graph. Subprocess keeps bench-run buildable
//! and runnable in CI even when llama-server isn't installed (the run
//! will fail at exec time, not at compile time).
//!
//! Requirements when actually running benches:
//!   - `micro-mind` on PATH or use --bin to point at a release build.
//!   - llama-server reachable per the main binary's discovery rules.

use anyhow::{Context, Result};
use clap::Parser;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use micro_mind::bench::summary::check_expectations;
use micro_mind::bench::{Fixture, TaskOutcome, parse_jsonl_file, summarize_trace};

/// Set by the SIGINT handler. Polled by `run_one`'s wait loop and by the
/// outer fixture loop. Best-effort — async-signal-safe stores only.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn sigint_handler(_: i32) {
    // Only async-signal-safe ops allowed in here.
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn install_sigint_handler() {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
    let action = SigAction::new(
        SigHandler::Handler(sigint_handler),
        SaFlags::empty(),
        SigSet::empty(),
    );
    // SAFETY: called exactly once from main() before any threads are spawned;
    // the handler only touches a static AtomicBool.
    unsafe {
        let _ = sigaction(Signal::SIGINT, &action);
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "bench-run",
    about = "Run micro-mind against bench fixtures and write JSONL traces"
)]
struct Cli {
    /// Directory containing fixture TOMLs. Default: bench/tasks.
    #[arg(long, default_value = "bench/tasks")]
    tasks: PathBuf,

    /// Output directory for traces and the run summary. Default: bench/runs/<timestamp>.
    #[arg(long)]
    out: Option<PathBuf>,

    /// Path to the micro-mind binary. Default: search PATH.
    #[arg(long, default_value = "micro-mind")]
    bin: PathBuf,

    /// Run each task this many times. Default 1.
    #[arg(long, default_value_t = 1)]
    reps: u32,

    /// Only run tasks whose id matches this substring.
    #[arg(long)]
    filter: Option<String>,

    /// Per-task timeout, seconds. Default 180.
    #[arg(long, default_value_t = 180)]
    timeout: u64,

    /// Working directory passed to micro-mind via -C. Default: cwd.
    #[arg(long, short = 'C')]
    cwd: Option<PathBuf>,

    /// Don't actually run — list what would run.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> Result<()> {
    install_sigint_handler();
    let cli = Cli::parse();
    let fixtures = Fixture::discover(&cli.tasks)
        .with_context(|| format!("discover fixtures in {}", cli.tasks.display()))?;
    let fixtures: Vec<Fixture> = match &cli.filter {
        Some(f) => fixtures.into_iter().filter(|x| x.id.contains(f)).collect(),
        None => fixtures,
    };
    if fixtures.is_empty() {
        anyhow::bail!("no fixtures found under {}", cli.tasks.display());
    }

    let out_dir = match cli.out {
        Some(p) => p,
        None => default_out_dir(),
    };
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("create out dir {}", out_dir.display()))?;

    println!(
        "bench-run: {} fixtures × {} reps → {}",
        fixtures.len(),
        cli.reps,
        out_dir.display()
    );
    if cli.dry_run {
        for fx in &fixtures {
            println!("  would run: {} ({})", fx.id, fx.description);
        }
        return Ok(());
    }

    let cwd = match cli.cwd {
        Some(c) => c,
        None => std::env::current_dir()?,
    };

    // Reject fixtures whose prompt could short-circuit the REPL we drive
    // by stdin (we send `<prompt>\n/quit\n` — a prompt containing a line
    // beginning with `/quit` / `/q` / `/exit` would terminate early and
    // produce a misleading trace).
    for fx in &fixtures {
        if prompt_has_repl_terminator(&fx.prompt) {
            anyhow::bail!(
                "fixture {}: prompt contains a line beginning with /quit /q or /exit, \
                 which would terminate the REPL before the task runs",
                fx.id
            );
        }
    }

    let mut all_outcomes: Vec<TaskOutcome> = Vec::new();
    let mut failures = 0u32;
    'outer: for fx in &fixtures {
        for rep in 0..cli.reps {
            if SHUTDOWN.load(Ordering::SeqCst) {
                eprintln!("bench-run: interrupted, stopping after current task");
                break 'outer;
            }
            let trace_path = out_dir.join(format!("{}-rep{rep}.jsonl", fx.id));
            print!("  {} rep {}/{} ", fx.id, rep + 1, cli.reps);
            let _ = std::io::stdout().flush();
            let run_started = Instant::now();
            let exec_result = run_one(
                &cli.bin,
                &cwd,
                &out_dir,
                &fx.prompt,
                cli.timeout,
                &trace_path,
            );
            let exec_ms = run_started.elapsed().as_millis();
            match &exec_result {
                Ok(final_answer) => {
                    let events = match parse_jsonl_file(&trace_path) {
                        Ok(e) => e,
                        Err(e) => {
                            println!("FAIL: trace parse error: {e}");
                            failures += 1;
                            continue;
                        }
                    };
                    let mut stats = summarize_trace(&events);
                    stats.final_answer = Some(final_answer.clone());
                    let fails = check_expectations(fx, &stats);
                    let passed = fails.is_empty();
                    if passed {
                        println!(
                            "ok  ({} ms, {} tools, {} tokens, {}ms run)",
                            stats.wall_ms, stats.tool_calls, stats.total_tokens, exec_ms
                        );
                    } else {
                        failures += 1;
                        println!(
                            "FAIL ({} ms, {} tools): {}",
                            stats.wall_ms,
                            stats.tool_calls,
                            fails.join("; ")
                        );
                    }
                    all_outcomes.push(TaskOutcome {
                        id: format!("{}-rep{rep}", fx.id),
                        trace_path: Some(trace_path.display().to_string()),
                        passed,
                        failures: fails,
                        stats,
                    });
                }
                Err(e) => {
                    failures += 1;
                    println!("FAIL: exec error: {e}");
                    all_outcomes.push(TaskOutcome {
                        id: format!("{}-rep{rep}", fx.id),
                        trace_path: Some(trace_path.display().to_string()),
                        passed: false,
                        failures: vec![format!("exec error: {e}")],
                        stats: Default::default(),
                    });
                }
            }
        }
    }

    // Write a summary JSON for downstream tooling.
    let summary_path = out_dir.join("summary.json");
    let summary_json = serde_json::json!({
        "schema_v": 1,
        "n_outcomes": all_outcomes.len(),
        "n_failures": failures,
        "outcomes": all_outcomes,
    });
    std::fs::write(&summary_path, serde_json::to_string_pretty(&summary_json)?)
        .with_context(|| format!("write summary {}", summary_path.display()))?;
    println!("summary → {}", summary_path.display());
    if failures > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Does any line of `prompt` start with a REPL-terminating slash command?
/// We strip a leading BOM and trim each line of leading whitespace before
/// checking.
fn prompt_has_repl_terminator(prompt: &str) -> bool {
    let normalized = prompt.strip_prefix('\u{feff}').unwrap_or(prompt);
    for line in normalized.lines() {
        let trimmed = line.trim_start();
        for term in ["/quit", "/q", "/exit"] {
            if trimmed == term
                || trimmed.starts_with(&format!("{term} "))
                || trimmed.starts_with(&format!("{term}\t"))
            {
                return true;
            }
        }
    }
    false
}

fn default_out_dir() -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    PathBuf::from(format!("bench/runs/{ts}"))
}

/// Spawn micro-mind, pipe prompt + /quit, wait for completion or timeout.
/// Returns the captured stdout as the "final answer" (best-effort).
fn run_one(
    bin: &Path,
    cwd: &Path,
    record_dir: &Path,
    prompt: &str,
    timeout_secs: u64,
    expected_trace: &Path,
) -> Result<String> {
    // We point `--record` at a per-task subdir so we can pick the resulting
    // file deterministically: each invocation creates exactly one JSONL.
    let rec_subdir = expected_trace.parent().unwrap_or(record_dir);
    let tmp_rec = rec_subdir.join(format!(
        ".rec-{}",
        expected_trace
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("task")
    ));
    std::fs::create_dir_all(&tmp_rec)?;

    let mut cmd = Command::new(bin);
    cmd.arg("-C")
        .arg(cwd)
        .arg("--record")
        .arg(&tmp_rec)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Put the child in its own process group so a single signal can take
    // out micro-mind *and* the llama-server it owns.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;
    let pgid = nix::unistd::Pid::from_raw(child.id() as i32);

    if let Some(mut stdin) = child.stdin.take() {
        writeln!(stdin, "{prompt}")?;
        writeln!(stdin, "/quit")?;
        // dropping stdin closes it
    }

    // Wait with a soft timeout. Crude but adequate for a v1 bench runner.
    let started = Instant::now();
    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGTERM);
            let _ = child.wait();
            anyhow::bail!("interrupted (SIGINT)");
        }
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if started.elapsed().as_secs() > timeout_secs {
                    let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGTERM);
                    // Brief grace, then SIGKILL the group if anyone's still alive.
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
                    let _ = child.wait();
                    anyhow::bail!("timeout after {timeout_secs}s");
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(e) => anyhow::bail!("wait failed: {e}"),
        }
    }

    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    // Move the one JSONL file produced into the expected trace path.
    let mut found = None;
    for entry in std::fs::read_dir(&tmp_rec)? {
        let p = entry?.path();
        if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            found = Some(p);
            break;
        }
    }
    if let Some(src) = found {
        std::fs::rename(&src, expected_trace).ok();
    } else {
        anyhow::bail!("no JSONL produced (subprocess output:\n{stdout})");
    }
    let _ = std::fs::remove_dir(&tmp_rec);
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_quit_on_its_own_line() {
        assert!(prompt_has_repl_terminator(
            "do a thing\n/quit\nthen another"
        ));
        assert!(prompt_has_repl_terminator("/q"));
        assert!(prompt_has_repl_terminator("/exit  with arg"));
    }

    #[test]
    fn accepts_quit_inside_normal_text() {
        // The model can talk about /quit; only a line that *begins* with
        // a terminator (after optional leading whitespace) is a problem.
        assert!(!prompt_has_repl_terminator("explain what /quit does"));
        assert!(!prompt_has_repl_terminator("plain prompt"));
    }
}
