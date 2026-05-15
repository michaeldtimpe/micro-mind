# Lessons Learned

A running log of mistakes, surprises, and hard-won insights from building micro-mind. Format borrowed from sibling projects ([`luxe/lessons.md`](https://github.com/michaeldtimpe/luxe/blob/main/lessons.md), [`neo-llm-bench/lessons.md`](https://github.com/michaeldtimpe/neo-llm-bench/blob/main/lessons.md)). Append new entries chronologically; do not edit historical entries except to add cross-references.

## Format

```
### [DATE] Short title

**What happened**: Description of the problem or surprise.

**Root cause**: Why it happened — the assumption that was wrong, the edge case missed, etc.

**Fix / takeaway**: What we did about it and the general principle for next time.

**Affected files**: Which parts of the codebase were involved.
```

---

## Entries

### [2026-05-14] The model gives up rather than retries — coaching hints are necessary but not sufficient

**What happened**: First live smoke against the assembled harness. Workflow 5/5 ("Find all uses of TODO in src/") failed: the model emitted `grep(path="/src/", pattern="TODO")` with a leading slash, `safe_path` correctly rejected it as outside the cwd, and the model responded with "I'm sorry, but I can't find any uses of TODO in src/. Could you please provide a relative path to the file?" — and stopped. Even after broadening the `escapes the working directory` coach hint to apply to all tools (not just `read_file`), the model on the next run produced the same broken `/src/` call, saw the same error, and gave up again, suggesting back the same wrong example (`"/src/main.rs"`) in its apology.

**Root cause**: Two compounding behaviours of this model size:
1. The model treats top-level project directories (`src`, `tests`) as if they were rooted at `/`, so it emits `/src/...` for what it means as `src/...`. This is a consistent mistake, not a one-off.
2. When a tool returns an error, `qwen25-1.5b-instruct` defaults to *explaining the error to the user* rather than *correcting and retrying*. The coach hint and the failure-memory injection both reached the model and were ignored. At this size, instruction-tuning toward "polite apologies" outranks instruction-tuning toward "try again with adjusted arguments".

**Fix / takeaway**: Broadened the coach hint *and* added a harness-level accommodation: `src/tools/fs_utils.rs::safe_path` now strips a single leading `/` if the absolute interpretation would escape the cwd but the relative form is valid. After this change, the same query (`Find all uses of TODO in src/`) succeeded — the model still emitted `/src/`, but `safe_path` quietly fixed it. The principle: **for a 1.5 B model, a deterministic harness accommodation beats a coaching hint every time.** Don't rely on the model to recover from a recoverable error if you can recognize the error shape and fix it before it surfaces.

**Affected files**: `src/tools/fs_utils.rs` (`safe_path` accommodation + test), `src/agent/coach.rs` (broadened `escapes the working directory` hint).

---

### [2026-05-14] Native `tool_calls` channel is reliable on this model — text recovery is belt-and-braces

**What happened**: Built `src/llm/client.rs` with two parsers: (1) native `message.tool_calls` from the OpenAI-compatible response, (2) text recovery from `<tool_call>{...}</tool_call>` or bare top-level JSON in the `content` field. Initial worry was that the 1.5 B model would frequently fall back to the text channel. Across all 5 live smoke workflows, the text-recovery path never fired. The model emitted clean native `tool_calls` JSON with empty `content` every time.

**Root cause**: `qwen25-1.5b-instruct` (Q8_0 with `--jinja`) uses Qwen's native chat template, which has structured tool-call slots. The BFCL bake-off (`neo-llm-bench`) measured 99.5 % native-channel usage on this model — our smoke testing matched.

**Fix / takeaway**: Keep the text-recovery path — it's cheap insurance (~50 LOC, 4 unit tests) for the 0.5 % case and for swapping in other quantizations later. But don't *design around* the text channel; the model's primary modality is structured calls. The unit tests in `src/llm/client.rs::tests` are sufficient to keep text recovery from regressing without burning live-smoke time on it.

**Affected files**: `src/llm/client.rs` (`recover_tool_calls_from_text`, `extract_first_balanced_json`).

---

### [2026-05-14] Cargo init's `edition = "2024"` is fine

**What happened**: `cargo init --name micro-mind --bin` produced a `Cargo.toml` with `edition = "2024"` (rustc 1.95.0). Briefly considered downgrading to `2021` for compatibility safety. Did not.

**Root cause**: micro-mind is a binary, not a library — there's no downstream consumer who cares about the edition. The edition affects only how `rustc` parses *this* crate's source. 2024 features (improved closure capture, refined `unsafe` rules) are useful and the toolchain pin is on the user's box.

**Fix / takeaway**: Don't pre-emptively downgrade language editions for binaries unless a specific compatibility need surfaces. The reflex to "play it safe" eats a small amount of leverage every time.

**Affected files**: `Cargo.toml`.

---

### [2026-05-14] `cargo init` creates `src/main.rs` and `Cargo.toml` — `Write` tool requires `Read` first

**What happened**: During the skeleton phase, attempted to overwrite `Cargo.toml` and `src/main.rs` immediately after `cargo init` created them, with the full intended contents. Both `Write` calls failed with `File has not been read yet. Read it first before writing to it.`

**Root cause**: The harness has a guard that the Write tool can only overwrite a file the agent has explicitly Read in this session. cargo init produced the files server-side; the agent never Read them. The guard is exactly the kind of "deterministic harness accommodation > model judgement" pattern micro-mind itself implements.

**Fix / takeaway**: When using tool-created files (`cargo init`, `npm init`, generators), Read them once before any Write, even if you intend to overwrite completely. The cost is one extra tool call; the saving is avoiding a class of accidental overwrites. (And it's a nice example of harness-level safety in another project — exactly the pattern we're porting here.)

**Affected files**: (process lesson — no files affected).

---

### [2026-05-14] `ureq` is the right call over `reqwest` for a single-threaded REPL

**What happened**: User feedback before implementation asked: "How do you feel about swapping reqwest for ureq?" Initial plan defaulted to `reqwest` (with blocking feature). Switched to `ureq` before writing any HTTP code.

**Root cause / takeaway**: `reqwest` brings tokio, hyper, tower, and a heavy TLS stack even with `default-features = false`. `ureq` is pure-Rust, blocking-by-design, no async runtime, ~1/10 the dep tree. For a single-threaded REPL talking to one local HTTP endpoint, `ureq` is strictly better on every axis that matters (binary size, dep build time, mental model). The release binary lands at 2.6 MB stripped — `reqwest` would have pushed this well past 5 MB.

**General principle**: Match the HTTP client to the concurrency model. Single-threaded blocking → `ureq`. Async server / many concurrent clients → `reqwest`. There's no "neutral default" — both choices are correct in their domain and wrong in the other.

**Affected files**: `Cargo.toml`.

---

### [2026-05-14] Write-aware elision is necessary — generic LRU elision would lose edit history

**What happened**: While porting `luxe`'s `context.py` to Rust, deliberated whether the elision strategy should be the same. luxe elides the oldest `role: tool` messages above a 70 % pressure threshold, keeping the 4 most recent. micro-mind ships with one critical change: **successful `write_file` / `edit_file` results are preserved verbatim through elision regardless of age.**

**Root cause**: Without this, the model forgets which files it has already edited and either re-edits them (re-introducing the same diff) or undoes prior work. The successful-write summary is small (~50 bytes) and reading it costs ~12 tokens — preserving them indefinitely is a near-free improvement on edit-coherence over multi-turn tasks.

**Fix / takeaway**: When elision-style context compression is in play, **identify which message classes are load-bearing for correctness and protect them.** For micro-mind: write summaries. For a benchmark harness like `neo-llm-bench`: probably the system prompt and the first user turn. For a long-form coding agent: probably the open file list. Generic LRU is wrong.

**Affected files**: `src/agent/context.rs` (`is_durable_write_result`, `elide_old_tool_results`).

---

### [2026-05-14] Compact tool rendering by default + `/last` / `/tool N` for expansion

**What happened**: First REPL output dumped the full `list_files_recursive` result inline — a 47-line block — into the user's terminal between the call announcement and the model's prose summary. Felt noisy.

**Root cause**: Default Claude-Code-style rendering shows tool outputs inline as they happen. At 1.5 B, the model's prose summary is often the most informative artifact in the turn, and the raw output is mostly clutter to a human reader.

**Fix / takeaway**: Render tool calls as one compact line (`▸ <name> <args>` / `└ ok <ms> <bytes>`). Provide `/last` (most recent tool's full output) and `/tool N` (arbitrary index) for explicit expansion. This makes drift detection easier — the user can scan a turn at a glance and reach for `/last` only when something looks wrong. Compactness in the harness is itself a debugging aid.

**Affected files**: `src/repl/render.rs`, `src/repl/mod.rs` (handlers for `/last` and `/tool`).

---

### [2026-05-14] The "5/5 smoke pass" criterion is informative even when it's met

**What happened**: Plan defined v1 ship criterion as "4/5 manual smoke workflows complete without manual intervention across two consecutive runs." On the first live run, we hit 4/5. The failing one (grep with `/src/`) was a real coachable failure mode. Fixed it; second run hit 5/5.

**Root cause / takeaway**: A pass criterion at a margin (4/5 instead of 5/5) is not "the bar is low" — it's "I expect one observable failure I can learn from on the first end-to-end run." That's a useful prediction. The `/src/` fix wouldn't have surfaced without the smoke; it wasn't in any of the 17 named risks from prior analysis. The bench-as-truth principle from `neo-llm-bench` applies here too: paper analysis is necessary but never sufficient. Always run the system end-to-end before declaring a feature complete.

**Affected files**: (process lesson). The fix from this session is captured separately in the `safe_path` leading-slash accommodation lesson above.

---
