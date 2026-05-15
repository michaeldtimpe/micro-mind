pub mod render;

use anyhow::Result;
use colored::Colorize;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::agent::{Session, run_turn};
use crate::config;

pub fn run(mut session: Session) -> Result<()> {
    println!(
        "{} {} {}",
        "micro-mind".bold(),
        format!("(qwen25-1.5b-instruct @ ctx={})", config::N_CTX).dimmed(),
        format!("— /help for commands").dimmed()
    );
    println!("{}", format!("cwd: {}", session.cwd.display()).dimmed());

    let mut rl = DefaultEditor::new()?;
    loop {
        let prompt = "> ".bold().to_string();
        let line = match rl.readline(&prompt) {
            Ok(s) => s,
            Err(ReadlineError::Interrupted) => {
                println!("(Ctrl-C — use /quit to exit)");
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        };
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(input);

        if let Some(cmd) = input.strip_prefix('/') {
            if handle_command(cmd, &mut session) {
                break;
            }
            continue;
        }

        if let Err(e) = run_turn(&mut session, input) {
            eprintln!("turn error: {e}");
        }
    }

    Ok(())
}

/// Returns true if the REPL should exit.
fn handle_command(cmd: &str, s: &mut Session) -> bool {
    let mut parts = cmd.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("").trim();
    let arg = parts.next().unwrap_or("").trim();
    match head {
        "quit" | "exit" | "q" => true,
        "reset" => {
            s.reset();
            println!("{}", "(conversation reset)".dimmed());
            false
        }
        "help" => {
            print_help();
            false
        }
        "tokens" => {
            let pct = (s.pressure() * 100.0) as u32;
            println!(
                "context: {} / {} tokens (≈{}% pressure)",
                crate::agent::context::estimate_messages_tokens(&s.messages),
                config::N_CTX,
                pct
            );
            false
        }
        "dump" => {
            for (i, m) in s.messages.iter().enumerate() {
                let role = m.role.as_str();
                let content_preview: String = m
                    .content
                    .as_deref()
                    .unwrap_or("")
                    .chars()
                    .take(120)
                    .collect();
                let tools = if !m.tool_calls.is_empty() {
                    format!(" tool_calls={}", m.tool_calls.len())
                } else {
                    String::new()
                };
                println!("[{i:02}] {role:10} {content_preview}{tools}");
            }
            false
        }
        "explain" => {
            print_explain(s);
            false
        }
        "last" => {
            match s.last_calls.last() {
                Some(call) => {
                    println!("--- last tool: {} ---", call.name);
                    println!(
                        "args: {}",
                        serde_json::to_string(&call.arguments).unwrap_or_default()
                    );
                    if let Some(err) = &call.error {
                        println!("ERROR:\n{err}");
                    } else {
                        println!("{}", call.result);
                    }
                }
                None => println!("(no tool calls yet)"),
            }
            false
        }
        "tool" => {
            let n: usize = arg.parse().unwrap_or(0);
            if n == 0 || n > s.last_calls.len() {
                println!("usage: /tool N   (N=1..{})", s.last_calls.len());
            } else {
                let call = &s.last_calls[n - 1];
                println!(
                    "--- tool {} of {}: {} ---",
                    n,
                    s.last_calls.len(),
                    call.name
                );
                if let Some(err) = &call.error {
                    println!("ERROR:\n{err}");
                } else {
                    println!("{}", call.result);
                }
            }
            false
        }
        other => {
            println!("unknown command: /{other}   (/help for list)");
            false
        }
    }
}

fn print_help() {
    let lines = [
        "/quit /exit /q      leave the REPL",
        "/reset              clear conversation (keeps llama-server warm)",
        "/tokens             show context pressure",
        "/dump               dump conversation buffer",
        "/explain            harness state (pressure, last stop, tools used)",
        "/last               full output of the most recent tool call",
        "/tool N             full output of tool call N (see /explain for list)",
        "/help               this message",
    ];
    for l in lines.iter() {
        println!("  {l}");
    }
}

fn print_explain(s: &Session) {
    let pct = (s.pressure() * 100.0) as u32;
    println!(
        "context pressure: {} / {} tokens (≈{}%)",
        crate::agent::context::estimate_messages_tokens(&s.messages),
        config::N_CTX,
        pct
    );
    println!(
        "cache: {} hits / {} misses ({} entries)",
        s.cache.hits,
        s.cache.misses,
        s.cache.entry_count()
    );
    println!("cwd: {}", s.cwd.display());
    println!(
        "tools loaded: {}",
        s.tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    match &s.last_stop {
        Some(r) => println!("last stop reason: {:?}", r),
        None => println!("last stop reason: (none yet)"),
    }
    match &s.last_usage {
        Some(u) => println!(
            "last usage: prompt={} completion={} total={}",
            u.prompt_tokens, u.completion_tokens, u.total_tokens
        ),
        None => println!("last usage: (not reported)"),
    }
    if s.last_calls.is_empty() {
        println!("last tool calls: (none)");
    } else {
        println!("last tool calls (newest last):");
        let start = s.last_calls.len().saturating_sub(5);
        for (i, c) in s.last_calls.iter().enumerate().skip(start) {
            let status = if c.is_ok() { "ok" } else { "ERR" };
            println!(
                "  #{:>2} {:>3} {:<24} {}ms {} bytes",
                i + 1,
                status,
                c.name,
                c.wall_ms,
                c.bytes_out
            );
        }
    }
}
