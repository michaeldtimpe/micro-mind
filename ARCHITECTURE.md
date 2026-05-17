# Architecture

A single Rust binary that simulates Claude Code's core development-assistant workflow on a 1.5 B-parameter local model. Three layers — prompt, tool, agent loop — each carrying mitigations for known failure modes of the model class.

## Module map

```
src/
├─ main.rs                clap CLI, server bootstrap, build_tool_surface, REPL, --record
├─ lib.rs                 library facet — re-exports obs + bench for the bench-* bins
├─ config.rs              all tunable constants (ctx, sampling, caps, thresholds)
├─ server.rs              llama-server lifecycle (singleton, attach-or-spawn)
│
├─ llm/
│  ├─ types.rs            ChatMessage, ToolCall, ChatRequest, ChatResponse, Usage
│  ├─ client.rs           ureq blocking client + native tool_calls + text recovery
│  └─ prompt.rs           system prompt (BFCL v2 + single-action + strict-output)
│
├─ tools/
│  ├─ mod.rs              ToolDef, ToolFn, ToolCallResult, dispatch, validate_args, hard_truncate
│  ├─ cache.rs            per-session memoization for read-only tools
│  ├─ fs_utils.rs         fuzzy_find, safe_path, canonicalize_path, walk_gitignore
│  ├─ fs_read.rs          read_file, list_dir, list_files_recursive, grep
│  ├─ fs_write.rs         write_file (atomic), edit_file (fuzzy + unique-match)
│  └─ shell.rs            bash with shlex + metacharacter reject + allowlist
│
├─ agent/
│  ├─ mod.rs              Session, run_turn — the core loop, StopReason, recorder threading
│  ├─ context.rs          estimate_tokens, pressure, write-aware elide
│  ├─ compress.rs         per-tool semantic summarizer (read_file, grep, bash, …)
│  ├─ coach.rs            error coaching + dispatch-path & guard-path failure-memory notes
│  └─ guards.rs           SemanticDedup, ReadTracker, WritePressure, length_truncation_note, first_turn_cold_read_check
│
├─ repl/
│  ├─ mod.rs              rustyline prompt + slash-command dispatch
│  └─ render.rs           compact tool-call / tool-result rendering
│
├─ obs/
│  ├─ mod.rs              re-exports + SCHEMA_V constant
│  └─ recorder.rs         Event variants, JsonlRecorder, NoopRecorder, Recorder trait
│
├─ bench/
│  ├─ mod.rs              re-exports
│  ├─ fixture.rs          Fixture / TaskExpect TOML schema + discovery
│  ├─ trace.rs            JSONL trace parser (additive-schema tolerant)
│  └─ summary.rs          summarize_trace, check_expectations
│
└─ bin/
   ├─ bench_run.rs        spawn micro-mind per fixture, capture traces, SIGINT-safe
   ├─ bench_replay.rs     offline trace validator (CI gate)
   ├─ bench_summarize.rs  text / markdown table over a directory of traces
   └─ bench_compare.rs    baseline vs candidate summary.json diff
```

Tests live alongside each module (`#[cfg(test)] mod tests`). 132 unit tests across the lib and the bench-* bins as of 2026-05-17 (guard-fire predicates + guard_failure_memory_note landed). Re-run `cargo test --release --all` for the live count; the breakdown drifts as modules grow.

## Runtime flow

```
                                    main.rs::main
                                         │
            ┌────────────────────────────┴──────────────────────────────┐
            │                                                           │
            ▼                                                           ▼
    server.rs::ServerHandle::attach_or_spawn                build_tool_surface(cwd)
            │   - honor LLAMA_SERVER_URL                              │
            │   - else probe :8080                                    ▼
            │   - else spawn llama-server with neo-llm-bench flags    Vec<ToolDef>
            │   - poll /health up to 60s                              (read_file, list_dir,
            ▼                                                          list_files_recursive,
       ServerHandle                                                    grep, write_file,
            │                                                          edit_file, bash)
            ▼
       LlmClient::new(server.url)
            │
            ▼
       Session::new(client, tools, cwd, prompt)
            │
            ▼
       repl::run(session)
            │
            ▼
       loop {
           rustyline.readline()
                │
                ├── starts_with('/') → handle_command()
                └── otherwise → agent::run_turn(state, input)
       }
```

The agent loop (`src/agent/mod.rs::run_turn`):

```
push user message
loop (max MAX_TURNS=8):
    if pressure > 0.7 → elide_old_tool_results (write-summaries preserved)
    record ChatRequest event
    response = client.chat(messages, tools)
    record ChatResponse event (finish_reason, usage, native + recovered tool_calls)
    push assistant message; remember its content as last_assistant_content
    if response.finish_reason == "length" → record Guard{length}, push concision note,
                                            set last_stop=Length, break (no dispatch)
    if response.tool_calls.is_empty() → render final answer, set last_stop=FinalAnswer, break

    for each tool_call:
        if SemanticDedup.record_and_check(name, args) → record Guard{dedup}, inject system
                                                       note, set last_stop=Dedup, break
        if (edit_file && !ReadTracker.has_seen(path))
        OR (write_file && path exists on disk && !ReadTracker.has_seen(path))
            → record Guard{read_before_write}, push tool-specific refusal stub,
              push coach::guard_failure_memory_note(tool, "read_before_write")
              if Some, continue
        if turn == 0 && read_file && path not in user_input
            → record Guard{cold_read}, push refusal stub,
              push coach::guard_failure_memory_note(tool, "cold_read") if Some
              (currently None — wiring is generic, opt-in per-kind in coach),
              continue
        record ToolCall event
        result = dispatch(name, args, …)              # tool layer enforces 8 KB cap
        coached = coach::coach(&result)               # prepend hint if error matches pattern
        push tool result message; record ToolResult event
        if let Some(summary) = compress::summarize(&result) → push as system note
        if result.error → push failure-memory system note
        if result.is_ok() → ReadTracker.record_read(name, args)
        if WritePressure.observe(name, ok, bytes) → record Guard{write_pressure}, Stop, return

record Stop event (turn, reason, wall_ms, final_answer=last_assistant_content)
```

Stop reasons: `FinalAnswer`, `TurnCap`, `WritePressure`, `Dedup`, `Length`, `Error(String)`.

## Layered survival primitives

### Prompt layer (`src/llm/prompt.rs`)

`system_prompt(cwd)` produces a fixed-template string under ~300 tokens. Five blocks:

1. **Identity** — "You are micro-mind, … operating inside `<cwd>`."
2. **Tool-use rules** (BFCL v2, lifted verbatim from `neo-llm-bench`):
   - `N` separate tool calls for `N` inputs (parallel rule)
   - If no tool satisfies the request, do not call any tool (decline rule)
   - Use Python operator syntax for math (formatting rule)
3. **Behaviour rules** (micro-mind original, derived from review):
   - Single-action bias
   - Emit tool call immediately, do not narrate
   - Read before modifying
   - After write, verify with ONE concise read/test, then stop
4. **Working directory** (re-stated as a closing reminder)

These rules address 4 of the 17 named failure modes: irrelevance over-call, parallel under-call, agentic drift, over-explaining prose.

### Tool layer (`src/tools/`)

#### Dispatch contract (`mod.rs`)

```rust
pub fn dispatch(
    name_raw: &str,
    arguments: &Value,
    tool_id: &str,
    tools: &HashMap<String, ToolDef>,
    cache: &mut ToolCache,
) -> ToolCallResult
```

Five guarantees:

1. **Name normalization**: whitespace stripped before lookup (`luxe` lesson — small models emit `"read_file\n"`).
2. **Schema validation**: required fields + primitive types before the function runs.
3. **Caching**: cacheable tools route through `ToolCache::get_or_run` with JSON-canonical keys.
4. **Crash containment**: a panicking tool produces a `ToolCallResult.error`, not a process crash.
5. **Hard output truncation**: every result is capped at 8 KB before it leaves the dispatch function. The marker `[truncated: N more bytes. Use grep / offset / max_bytes for more.]` tells the model how to retrieve more.

#### Tool surface

See `README.md §Survival primitives at a glance` for the full table.

#### Atomic write contract (`fs_write.rs::atomic_write`)

```
1. tmp_path = parent_dir / ".<filename>.tmp.<pid>.<nano>"
2. open(tmp_path) with O_CREAT|O_EXCL|O_WRONLY
3. write_all(bytes)
4. fsync(fd)
5. drop(fd)
6. rename(tmp_path, dest_path)
```

If steps 1–5 fail, `dest_path` is untouched. If step 6 fails (e.g. disk full), the tmp file remains; the next successful write supersedes it. `fs_write.rs::tests::atomic_write_does_not_leave_tmp` verifies the happy path.

#### Fuzzy match contract (`fs_utils.rs::fuzzy_find`)

Normalize both haystack and needle:

- CRLF → LF
- Trailing whitespace stripped per line

Then exact-string-match the normalized forms; map the match position back to the original haystack coordinates. Returns `FuzzyMatch { start, end, extra_matches }`.

`edit_file` rejects `extra_matches > 0` unless `replace_all=true`. The trailing-whitespace + CRLF tolerance is the single biggest reduction in 1.5 B edit-failure rate.

#### Shell hardening (`shell.rs`)

1. `shlex::split` the command. Reject if it can't parse.
2. For each token, reject if it contains any of: `|`, `>`, `>>`, `<`, `&`, `&&`, `||`, `;`, `$(`, `` ` ``.
3. First token must be in the allowlist (~20 binaries).
4. For `python`/`python3`/`node`: reject if any subsequent token is `-c` or `-e` (silent allowlist bypass).
5. `current_dir(cwd)`, `stdin(null)`, `stdout/stderr(piped)`.
6. Poll `try_wait()` against a deadline; SIGKILL on timeout.
7. 8 KB output cap (defense-in-depth — `dispatch::hard_truncate` already caps).

### Agent-loop layer (`src/agent/`)

#### Semantic dedup (`guards.rs::SemanticDedup`)

Captures call attempts in a `VecDeque`. Normalizes each call:

1. Trim whitespace on tool name.
2. Recursively trim string args.
3. For path-shaped fields (`path`, `file`, `directory`, `dir`): apply `canonicalize_path` (strip `./`, collapse `//`, trim trailing slash).
4. Sort JSON object keys (`serde_json::Map` preserves insertion order; we sort + rebuild).

Then hash the canonical string. If the last `DEDUP_CONSECUTIVE_LIMIT=3` entries are identical, the guard fires: a system note is injected and the loop breaks for this assistant turn. Test coverage: dedup catches `src/main.rs` / `./src/main.rs` / `src//main.rs` as the same call.

#### Read-before-write (`guards.rs::ReadTracker`)

Records `(canonicalize_path(arg.path))` for every successful `read_file` / `list_dir` / `list_files_recursive` / `grep`. On a write/edit, checks:

- exact match on the target path, **or**
- match on any prefix (parent directory was listed), **or**
- `.` was scanned (the model has surveyed the layout)

Two variants of the gate by tool:

- `edit_file`: always enforced. Editing content blind is the failure mode we care about.
- `write_file`: only enforced when the target *already exists on disk*. Brand-new files skip the gate — the model isn't modifying anything, and forcing a "read it first" on a non-existent file makes the 1.5 B model interpret the refusal as "the file doesn't exist" and stop (the polite-apology failure documented 2026-05-14).

When the gate fires, the refusal stub is also tool-specific: `edit_file` → "read it first via read_file"; `write_file` → "survey the directory first via list_dir". Different recovery paths.

In addition to the refusal stub, the `continue` branch pushes a system-role failure-memory note via `coach::guard_failure_memory_note(tool, "read_before_write")` — the analog of the dispatch-path `failure_memory_note(call)` injection. The dispatch-path note is what drives placeholder-rejection recovery on 9/10 reps (fixture `11-write-file-placeholder`); wiring the same affordance into the `continue` branch lifted `read_before_write` recovery from 0/3 to first-hop-recovery 3/3 on fixture `12-edit-file-read-or-write`. Closing the *second* hop (model emitting the `edit_file` after the recovery `read_file`) is multi-turn-chain territory the 1.5 B model can't sustain, and is left as future work — see `lessons.md` 2026-05-17.

#### First-turn cold-read (`guards.rs::first_turn_cold_read_check`)

Refuses `read_file` on turn 0 when the path (or its basename) doesn't appear in the user's input. Path `.` is exempt (project survey is always legit). `grep` / `list_dir` / `list_files_recursive` are exempt — they're legitimate exploration tools with generic search paths.

Catches the BFCL "spurious tool call on self-answerable prompt" failure: on "What is 17 + 25?" the model emits `read_file("/dev/null")` to satisfy the tool channel, then answers correctly on turn 1. The cost is ~1100 wasted prompt tokens (the system prompt + tool defs re-echoed). Guard intercepts before dispatch; the model's deterministic chain still produces the correct answer on turn 1 from the refusal stub.

Substring match is case-insensitive against the canonicalized path and its basename. False positives result in a recoverable refusal — the model gets the note and can retry with a path the user referenced or answer directly.

Like the `read_before_write` branch, this `continue` path also calls `coach::guard_failure_memory_note(tool, "cold_read")` — the function returns `None` today (the refusal stub already steers toward "answer the user directly"), so the call is a no-op, but the wiring is in place so a future kind that benefits from a failure-memory nudge opts in by adding its case to the `match` in `coach.rs`.

#### Write-pressure exit (`guards.rs::WritePressure`)

Counts successful `write_file`/`edit_file` calls. After the first write succeeds, every subsequent **zero-byte non-write** tool result (typical pattern: model keeps `read_file`-ing an already-confirmed file) increments a streak. Streak ≥ `WRITE_PRESSURE_ZERO_BYTE_LIMIT=3` breaks the loop — the model has effectively finished but is spinning. Any non-zero result resets the streak.

#### Context elision (`context.rs::elide_old_tool_results`)

Triggered when `estimate_messages_tokens(messages) / 8192 > 0.7`. Algorithm:

1. Identify all `role: tool` messages in order.
2. If count ≤ `keep_recent=4`, return unchanged.
3. Otherwise, elide all but the 4 most recent — **except** successful `write_file` / `edit_file` results, which are preserved verbatim regardless of age.

Elision replaces the message content with `[elided: <name> -> N bytes]`. The model still sees the call happened; just not what it returned.

This is the single most important survival primitive at 8192 ctx. Without elision, a 24 KB `read_file` early in the turn occupies ~3 KB of context for the rest of the conversation.

#### Tool-result compressor (`compress.rs::summarize`)

After each tool call, emits a one-line semantic summary as a system note alongside the raw result. Examples:

```
read_file src/main.rs       → 412 lines, 18234 bytes, defines main
grep /TODO/ src             → 12 matches in 6 files
bash `cargo check`          → OK (exit=0 234ms)
bash `cargo test`           → FAIL (exit=101 1.2s), 37 passed, 1 failed
list_files_recursive .      → 17 entries, mostly *.rs
write_file src/repl/mod.rs  → write_file ok: src/repl/mod.rs (4821 bytes)
edit_file src/agent/mod.rs  → edit_file ok: src/agent/mod.rs (1 replacement)
```

The model sees both: the raw bytes-capped result *and* this compressed restatement. Tiny models respond disproportionately well to compressed state vs raw token sludge — verified by smoke testing (the model's final answers consistently reference the compressed numbers, not the raw output).

#### Coach (`coach.rs::coach`)

Inspects errors and bash non-zero exits for known patterns; appends a hint after the raw error. Currently 9 patterns:

| Trigger | Hint |
|---|---|
| `edit_file: could not find` | "Read the file again — the snippet may differ in whitespace…" |
| `edit_file: matched N times` | "Make snippet longer/unique, or set replace_all=true." |
| `write_file: placeholder` | "Replace placeholder markers with the real implementation." |
| `write_file: accidental wipe` | "Use edit_file targeting the specific text." |
| any tool, `escapes the working directory` | "Use a relative path — no leading slash, no `../`." |
| `bash: metacharacter` | "One command at a time, no pipes/redirects/chaining." |
| `bash: not allowed` | "Binary not in allowlist; use list_dir/read_file/grep." |
| `bash: timeout` | "Increase timeout_s or narrow the command." |
| stderr: `unrecognized option`, `command not found`, `No such file`, `Permission denied` | situational hints |

Also injects a synthetic system-role "do not repeat the same call unchanged" note after every error result, via two parallel functions:

- `coach::failure_memory_note(call: &ToolCallResult) -> Option<String>` — called from the dispatch path. Returns `Some` iff the call carried an error. This is what makes placeholder-rejection recovery work on `11-write-file-placeholder`.
- `coach::guard_failure_memory_note(tool: &str, kind: &str) -> Option<String>` — called from the `continue`-style guard branches that short-circuit before reaching `dispatch`. Keyed off the guard kind: returns `Some` only for kinds where retry-with-different-shape is the productive outcome. Today that's just `read_before_write`; `cold_read` / `dedup` / `write_pressure` / `length` / `turn_cap` all return `None`. New kinds opt in by adding their case to the `match` — the call site in `agent/mod.rs` is generic.

#### Length-truncation exit (`agent/mod.rs` + `guards.rs::length_truncation_note`)

When llama-server reports `finish_reason="length"`, the assistant message is structurally incomplete (any tool_calls in it may have unbalanced JSON). The loop:

1. Records a `guard` event of `kind=length`.
2. Pushes `guards::length_truncation_note()` as a system message — persists into the next user turn so the model sees "your previous response was cut off, be more concise".
3. Sets `last_stop=StopReason::Length` and breaks **without dispatching any tool_calls**.

The truncated message itself is still pushed into history (so the user sees what got generated), but treated as a dead end for control flow.

### Observability layer (`src/obs/`)

The agent loop emits JSONL events when `--record <dir>` is passed:

- `session_start` — once at REPL startup. Carries `schema_v` (currently 2).
- `chat_request` — pre-POST. `turn`, `n_messages`, `n_tools`.
- `chat_response` — post-decode. `finish_reason`, `wall_ms`, native + recovered tool_call counts, OpenAI-style `usage` (prompt/completion/total tokens).
- `tool_call` / `tool_result` — every dispatch, before and after. `wall_ms`, `bytes_out`, `cached`, `error`.
- `guard` — every guard fire. `kind` ∈ {`dedup`, `read_before_write`, `cold_read`, `write_pressure`, `length`, `turn_cap`}. Counted by kind in `Summary::guards_by_kind` and totalled in `Summary::guard_fires`; both are exposed as fixture predicates (`must_fire_guards` / `must_not_fire_guards` / `min`/`max_guard_fires`).
- `stop` — end of `run_turn`. Carries `final_answer` (v2) — the last non-empty assistant content, used by `bench-replay` to validate `expect.must_contain` offline.

Full schema in `obs/schema.md`. The recorder is a trait; the default is `NoopRecorder` (zero-cost when recording is disabled). `JsonlRecorder` is best-effort — a failed write is logged once to stderr and subsequent events are dropped, never breaking the run.

## Configuration

All tunable constants live in `src/config.rs`. Highlights:

```rust
N_CTX = 8192                            // matches qwen25-1.5b-instruct.yaml
TEMPERATURE = 0.0   TOP_P = 1.0   REPEAT_PENALTY = 1.1   SEED = 42

MAX_TURNS = 8                           // hard cap before loop breaks
PRESSURE_THRESHOLD = 0.7                // elision trigger
KEEP_RECENT_TOOLS = 4                   // preserved through elision
WRITE_PRESSURE_ZERO_BYTE_LIMIT = 3      // streak before exit
DEDUP_CONSECUTIVE_LIMIT = 3             // SemanticDedup trip count

TOOL_OUTPUT_HARD_CAP = 8 * 1024         // applies to every tool result
READ_FILE_DEFAULT_MAX = 24 * 1024
READ_FILE_HARD_MAX = 64 * 1024
READ_FILE_REFUSAL_THRESHOLD = 256 * 1024
LIST_DIR_CAP = 200
LIST_RECURSIVE_CAP = 500   LIST_RECURSIVE_DEFAULT_DEPTH = 3
GREP_MAX_MATCHES_DEFAULT = 50
```

Changing any of these implicitly changes the model's behavior envelope. Don't change without a falsifiable test.

## Non-goals

Decisions deliberately made for v1:

- **No sub-agents.** Multi-turn floor on this model is 0 % (`neo-llm-bench` BFCL multi-turn). Sub-agent orchestration burns context without buying capability.
- **No plan mode.** Same reason. Single-action bias is doing the work plan mode would do at a higher quality.
- **No MCP.** Adds tool surface; on a 1.5 B model every new tool increases routing entropy on the irrelevance axis.
- **No tree-sitter symbols / BM25 search.** `luxe` has these and uses them well at 35 B. At 1.5 B, the system prompt would have to describe them, eating ctx and increasing routing entropy.
- **No `glob` tool.** `list_files_recursive` + `grep` cover the use cases with less routing confusion (`glob` vs `list_files_recursive` vs `list_dir` is a coin flip on this model).
- **No multi-file refactors.** Read-before-write enforcement makes them possible in principle but the model's state-tracking limit hits before the diff converges.
- **No streaming output.** Nice but not load-bearing. The render layer is structured around final messages, not chunks.
- **No async / tokio.** Single-threaded REPL. `ureq` is the HTTP client.

## Build + test

```bash
cargo build               # debug
cargo build --release     # ~2.6 MB stripped (micro-mind), bench-* bins also produced
cargo test                # 132 unit tests, no model required
cargo clippy -- -D warnings   # gating, zero-warning floor
cargo fmt --all --check       # gating
```

End-to-end smoke (requires `llama-server` running on port 8080):

```bash
LLAMA_SERVER_URL=http://127.0.0.1:8080 \
  printf 'List the files in src\n/quit\n' | ./target/release/micro-mind
```

Bench loop (also requires `llama-server`):

```bash
LLAMA_SERVER_URL=http://127.0.0.1:8080 \
  ./target/release/bench-run --bin ./target/release/micro-mind --reps 3
```

CI is hermetic: no llama-server, no GPU. The chain there is `cargo fmt
--check` → `cargo clippy -D warnings` → `cargo test --all-targets` → schema
validate the sample trace → replay sample trace against sample fixture →
summarize sample trace → **gating replay** of `bench/baselines/main/`
(every predicate must hold; currently 12 fixtures × 3 reps = 36/36) →
**advisory replay** of every directory under `bench/baselines/archive/`
(`continue-on-error: true`; historical drift surfaces but doesn't gate).

See `README.md §Quick start` and `bench/README.md` for the full matrix.
