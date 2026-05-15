# Claude Code instructions for micro-mind

Auto-loaded at session start. Points at the durable design contracts and the short list of project-specific gotchas.

## Single-model policy

**micro-mind pins exactly one model: `qwen25-1.5b-instruct`** (`~/models/Qwen2.5-1.5B-Instruct-Q8_0.gguf`, GGUF Q8_0, ~1.9 GB). Picked by [`neo-llm-bench`](https://github.com/michaeldtimpe/neo-llm-bench) on 2026-05-14 against the non-dominated quadrilateral. Lead survives matched-ID correction; decline weakness is steerable at the system layer. **Do not introduce model fan-out**: no per-task model selection, no router, no A/B against another model unless the user explicitly asks for a re-bench against `neo-llm-bench`'s finalists.

The champion path is locked end-to-end:

- **Runtime**: `llama.cpp` via `llama-server`. Metal offload. OpenAI-compatible HTTP API at `/v1/chat/completions`.
- **Quant**: Q8_0 weights, Q8_0 KV cache.
- **Sampling**: `temperature=0.0`, `top_p=1.0`, `repeat_penalty=1.1`, `seed=42`, `max_tokens=2048`. Deviating from these costs accuracy — see `lessons.md` (forthcoming) when it accumulates.

## Project shape

A Rust binary + a thin library facet. Entry point `src/main.rs`. Library
crate (`src/lib.rs`) exposes only the observability schema and the bench
helpers so the four `bench-*` bins can reuse them without dragging in
`agent/tools/repl`. Talks to `llama-server` over HTTP via `ureq` (blocking,
no tokio — the REPL is single-threaded and the 80 MB tokio runtime weight
isn't worth it on a tool meant to leave headroom for the 1.6 GB model).

Module map (read `ARCHITECTURE.md` for the long version):

```
src/
├─ main.rs                clap CLI, build_tool_surface, REPL bootstrap
├─ lib.rs                 library facet (pub mod bench, obs)
├─ config.rs              all numeric defaults (ctx, sampling, caps)
├─ server.rs              llama-server lifecycle (singleton)
├─ llm/                   chat client, types, system prompt
├─ tools/                 7-tool surface + dispatch + cache + fs_utils
├─ agent/                 run_turn loop + guards + context + compress + coach
├─ repl/                  rustyline UI + compact rendering
├─ obs/                   Recorder trait + JSONL recorder (schema: obs/schema.md)
├─ bench/                 Fixture/Summary types, trace parser, expectation checks
└─ bin/                   bench-run, bench-summarize, bench-replay, bench-compare

bench/
├─ tasks/                 *.toml fixtures (one task per file)
├─ baselines/             checked-in historical runs (dir-per-run, see README)
└─ samples/               sample trace + fixture exercised by CI
```

Observability is opt-in: pass `--record <dir>` to the main binary to append
JSONL events for the session. CI is hermetic — it exercises `bench-replay`
+ `bench-summarize` against `bench/samples/`, plus an advisory replay of
every committed baseline. No llama-server required at any CI step.
`clippy -D warnings` and `cargo fmt --check` both gate.

## Architecture: layered survival primitives

The 1.5 B model can do **one** competent tool hop and decline irrelevant requests. It cannot reliably plan, recover from failure, or track state across many turns. Every architectural choice trades model capability for harness capability.

Three layers of mitigation:

1. **Prompt layer** (`src/llm/prompt.rs`) — BFCL v2 anti-over-call + parallel + math rules, plus single-action bias and strict-output behaviour.
2. **Tool layer** (`src/tools/`) — fuzzy edit-matching, atomic writes, honesty guards, shell metacharacter rejection, 8 KB hard output cap, large-file refusal.
3. **Agent loop layer** (`src/agent/`) — semantic dedup, read-before-write enforcement, write-pressure exit, length-truncation exit, failure-memory injection, write-aware context elision, per-tool semantic summarization, harness-level error coaching.

Stop reasons emitted by `run_turn` (all surfaced in `obs/schema.md` and as fixture predicates): `FinalAnswer`, `TurnCap`, `WritePressure`, `Dedup`, `Length`, `Error: …`.

When the model fails a task, the **first** question is "which layer should catch this?" Not "is the model dumb?". The 5 smoke workflows passed because of layered mitigations, not because the model is good.

## When working on this repo

1. **The harness is smarter than the model.** Don't add prompt-level requests when a tool-layer or agent-layer guard can do it deterministically. Prompt tokens are expensive (8192 ctx) and probabilistic; code is cheap and certain.
2. **Tools are mostly read-only.** `read_file`, `list_dir`, `list_files_recursive`, `grep`, `bash` are cached. Only `write_file` and `edit_file` mutate. Honesty guards live on those two.
3. **Every new tool costs routing entropy.** The 1.5 B model already over-calls on irrelevance (~57 %). Adding a tool means the model is choosing between N+1 options instead of N. Default to "no" on new tools; default to "add a flag to an existing tool" instead. We dropped `glob` from v1 for exactly this reason.
4. **`temp=0.0` is mandatory.** No exceptions. The `neo-llm-bench` t=0.7 swing is 10 pp on HumanEval; it will surface as fixture flakiness here too.
5. **`llama-server` is a singleton.** `/reset` clears the conversation, not the server. Don't add `restart-on-reset` — cold-start latency kills iteration.
6. **No async, no tokio.** `ureq` is the HTTP client. If you need parallelism for a future feature, the right answer is a separate worker thread, not a runtime.

## Bench / replay bins

| Bin | Purpose |
|---|---|
| `bench-run` | Drive `micro-mind` as a subprocess against every fixture in `bench/tasks/*.toml`, write per-rep JSONL traces + `summary.json`. Refuses fixtures whose prompt could short-circuit the REPL on stdin. Needs a working llama-server. |
| `bench-summarize` | Aggregate one or more JSONL traces into a text or markdown table (tools, tokens, wall_ms, stop). |
| `bench-replay` | Validate a trace against a fixture *without* running the model — the CI gate. Schema-only mode is also supported. |
| `bench-compare` | Diff a candidate `summary.json` against a baseline; exits 1 on outcome regression, 2 on soft regression (latency/tokens beyond `--wall-pct` / `--tokens-pct`). |

As of schema v2, the `stop` event carries `final_answer`, so `bench-replay` can fully validate `expect.must_contain` from a trace alone. Pre-v2 traces (no `schema_v` field on `session_start`) still fail-closed on that predicate unless bench-run's stdout capture fills it. See `obs/schema.md` for the version policy.

## Tool surface decisions

| Tool | Why this name / shape |
|---|---|
| `read_file` | `offset`+`max_bytes` mandatory. Default 24 KB, hard 64 KB. Anything bigger and the model loses the plot. Large-file refusal: >256 KB without offset → refuse and direct the model to `grep`. |
| `list_dir` | Non-recursive. Capped at 200 entries. |
| `list_files_recursive` | `.gitignore`-aware via the `ignore` crate. Depth-capped (3 default). 500-entry cap. **This is the project-map tool** — give the model orientation in one call instead of N `list_dir`s. |
| `grep` | Regex via the `regex` crate. Returns `file:line:match` lines. Cap 50 matches by default; 8 KB output cap. |
| `write_file` | Atomic (temp file + fsync + rename). Honesty guards: reject placeholders (`<your code here>`, `// TODO: fill in`, etc.) and mass deletion (>1 KB file → <100 B new content). |
| `edit_file` | **Fuzzy match by default** (CRLF + trailing whitespace tolerant). Unique-match enforced; `replace_all=true` is the explicit escape hatch. The single highest-value tool decision — exact-string-match `edit` is a loop generator on 1.5 B models. |
| `bash` | Allowlisted (~20 binaries). No pipes, redirects, `&&`, `;`, `$(...)`, backticks. No `-c`/`-e` flags for python/node (silent allowlist bypass). Output 8 KB cap. |

## Common task patterns

### "Add a new tool"

Don't, by default. Ask the user whether an existing tool can be extended instead. If a new tool is really warranted:

1. Define it in `src/tools/<area>.rs` returning `ToolDef` via `ToolDef::new(...)`.
2. Mark `.cacheable()` only if the call is referentially transparent (same args → same result modulo the filesystem).
3. Add to `build_tool_surface()` in `src/main.rs`.
4. Add a semantic-summary handler in `src/agent/compress.rs::summarize`.
5. Add an error-coach hint in `src/agent/coach.rs::hint_for_error` for the failure shapes you can predict.
6. Unit tests in the same file as the tool.

### "Change the system prompt"

`src/llm/prompt.rs::system_prompt`. Keep it under 1500 chars (≈300 tokens). Every token here costs one less token of tool-output budget at 8192 ctx. Pinned tests verify the BFCL v2 anti-over-call rule, the parallel rule, and the read-before-write line are present — keep them.

### "Wire up a new failure mode you saw in real use"

1. Reproduce in a smoke test (pipe a query into the binary).
2. Add a guard in `src/agent/guards.rs` if it's behavioural (dedup-style), a coach hint in `src/agent/coach.rs` if it's recoverable diagnostics, a tool-layer rejection in `src/tools/<area>.rs` if it's input validation.
3. Update the README's "Survival primitives at a glance" table.
4. Note in `lessons.md` if it changed how you'd design something next time.

## Things that have bitten us

(This section will grow.)

- **Model emits absolute paths for relative ones.** `qwen25-1.5b-instruct` emitted `/src/` when it meant `src/`. The original `safe_path` rejected this cleanly, but the model didn't retry with the corrected form — it gave up. Harness now strips a single leading `/` if the absolute interpretation falls outside the cwd. (`src/tools/fs_utils.rs::safe_path` 2026-05-14)
- **The model will not retry on its own.** Failure-memory injection ("do not repeat the same call") is necessary but insufficient. If a tool fails, the model frequently just apologizes and stops. Harness-level accommodations (like the leading-slash fix) are stronger than coaching hints, because they remove the failure entirely.
- **8 KB hard output cap matters.** A single 40 KB `read_file` poisons the rest of the turn at 8192 ctx. The tool layer caps before the agent loop sees the result.
- **Length truncation is recoverable, but only as a turn boundary.** When llama-server reports `finish_reason="length"` we don't dispatch the model's truncated tool_calls (they may be incomplete) — we break with `StopReason::Length` and push a "be more concise" system note into the conversation so the *next* user turn nudges the model toward shorter output. (`src/agent/mod.rs` 2026-05-15)
- **Bench fixtures are aspirations, not ground truth.** The committed `2026-05-15-main` baseline has 3/9 reps passing. Failures (`01-read-readme` over the token cap, `03-decline-irrelevant` over-calls and overshoots) are real model behaviour, captured to ratchet against. Don't loosen fixture predicates to make baselines green — improve the harness or the model context.

## Memory

User-level memory for this project lives in `~/.claude/projects/-Users-mtimpe-Downloads-micro-mind/memory/`. Read at session start if it exists.

## Related projects

- [`michaeldtimpe/luxe`](https://github.com/michaeldtimpe/luxe) — 35 B MoE agentic harness, MLX-only. Source of the design primitives `micro-mind` ports to Rust.
- [`michaeldtimpe/neo-llm-bench`](https://github.com/michaeldtimpe/neo-llm-bench) — the bake-off that picked `qwen25-1.5b-instruct`. Source of the BFCL v2 prompt and the failure-mode taxonomy.
