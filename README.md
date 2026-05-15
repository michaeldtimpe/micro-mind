# micro-mind

A Claude Code-style interactive development REPL powered by `qwen25-1.5b-instruct` via `llama-server`. Sister project to [`michaeldtimpe/luxe`](https://github.com/michaeldtimpe/luxe) (MLX-only, 35 B MoE) and [`michaeldtimpe/neo-llm-bench`](https://github.com/michaeldtimpe/neo-llm-bench) (the bake-off that picked this model).

> **Status:** v1 + observability. End-to-end smoke 5/5 across the canonical workflows (list, read, grep, decline-irrelevance, decline-math). 106/106 unit tests passing. Release binary 2.6 MB stripped. Schema v2 JSONL traces, four bench binaries, first committed baseline (`bench/baselines/2026-05-15-main/`: 3/9 reps pass — see CLAUDE.md for what the failures tell you). See `RESUME.md` (forthcoming) for active state.

## Why this exists

`neo-llm-bench` picked `qwen25-1.5b-instruct` as the champion small-model tool-use substrate (77.1 % BFCL matched, +8 pp over runner-up, outside CI). `luxe` is a working agentic harness that's been hardened against the failure modes of capable local models. **micro-mind takes the harness lessons from `luxe` and re-targets them at the 1.5 B model, in Rust, for minimum-RAM operation.**

The central design choice — review-confirmed and verified by the smoke tests:

> Make the harness smarter instead of pretending the model is smarter.

## What it does

```
micro-mind                                     # spawn llama-server, attach REPL
  ↓
> Read main.rs and tell me what it does.
  ↓
qwen25-1.5b-instruct emits structured tool_calls
  ↓
agentic loop, 7 survival primitives layered on top
  ↓
final assistant message, or guard fires (turn cap / dedup / write-pressure)
```

Single capable 1.5 B model. No sub-agents, no plan mode, no MCP — multi-turn floor on this model size is 0 % (`neo-llm-bench` BFCL multi-turn). Adding orchestration burns context without buying capability.

## What you get

- Interactive REPL, Claude Code-style inline tool blocks
- 7-tool surface: `read_file`, `list_dir`, `list_files_recursive`, `grep`, `write_file`, `edit_file`, `bash`
- Anti-fail-mode harness: see [`ARCHITECTURE.md`](ARCHITECTURE.md) for the 17 named risks and where each one is mitigated.
- ~2.6 MB stripped release binary, single thread, no tokio.

## Requirements

- Apple Silicon Mac (Metal-accelerated `llama.cpp`)
- `llama-server` from a local `llama.cpp` build
- `Qwen2.5-1.5B-Instruct-Q8_0.gguf` (~1.9 GB; the exact file `neo-llm-bench` benchmarked on)
- Rust 1.85+ (edition 2024)

## Install

```bash
git clone https://github.com/michaeldtimpe/micro-mind.git
cd micro-mind
cargo build --release
```

The binary lands at `target/release/micro-mind`.

## Quick start

Option A — let micro-mind spawn `llama-server`:

```bash
./target/release/micro-mind
# defaults:
#   bin   = /Users/mtimpe/code/llama.cpp/build/bin/llama-server
#   model = ~/models/Qwen2.5-1.5B-Instruct-Q8_0.gguf
#   port  = 8080
```

Override paths via `MICROMIND_LLAMA_SERVER` and `MICROMIND_MODEL_PATH`.

Option B — run `llama-server` yourself, attach:

```bash
llama-server -m ~/models/Qwen2.5-1.5B-Instruct-Q8_0.gguf \
  --ctx-size 8192 --n-gpu-layers 99 --threads 6 \
  --batch-size 512 --ubatch-size 512 \
  --cache-type-k q8_0 --cache-type-v q8_0 --jinja --port 8080 &

LLAMA_SERVER_URL=http://127.0.0.1:8080 ./target/release/micro-mind
```

## Production model

The model and sampling config are pinned to match `neo-llm-bench`'s champion bake-off exactly:

```yaml
model:    Qwen2.5-1.5B-Instruct-Q8_0.gguf
n_ctx:    8192
n_gpu_layers: 99
threads:  6
batch:    512 / ubatch 512
kv-cache: q8_0 (k+v)
sampling: temperature=0.0  top_p=1.0  repeat_penalty=1.1  seed=42  max_tokens=2048
```

`temperature=0.0` is mandatory. `HumanEval pass@1` falls 10 pp on this model at `t=0.7` (`neo-llm-bench` rep_0). `repeat_penalty=1.1` is the only deviation from BFCL bake-off sampling — small models occasionally token-loop at `1.0`.

## CLI

```
micro-mind                                # attach/spawn, then REPL
micro-mind -C <dir>                       # operate inside <dir> instead of $PWD
micro-mind --no-spawn                     # error if no LLAMA_SERVER_URL — never spawn
micro-mind --record <dir>                 # append-only JSONL telemetry (see obs/schema.md)
```

REPL commands:

```
/quit /exit /q      leave the REPL
/reset              clear conversation (keeps llama-server warm)
/tokens             show context pressure
/dump               dump conversation buffer
/explain            harness state (pressure, last stop, tools used, cache stats)
/last               full output of the most recent tool call
/tool N             full output of tool call N
/help               command list
```

## Survival primitives at a glance

| Failure mode (from `neo-llm-bench` / `luxe`) | Mitigation | Location |
|---|---|---|
| Irrelevance over-call | BFCL v2 anti-over-call rule | `src/llm/prompt.rs` |
| Parallel under-call | BFCL v2 parallel rule | `src/llm/prompt.rs` |
| Agentic drift (`inspect→inspect→…`) | Single-action bias + verify-once-stop | `src/llm/prompt.rs` |
| Literal-dedup-evading whitespace/path mutations | Semantic dedup (whitespace + path canon + JSON key order) | `src/agent/guards.rs` |
| Retrying the same broken call indefinitely | Tool-failure memory injection | `src/agent/coach.rs` |
| edit_file whitespace mismatch loops | Fuzzy match (CRLF + trailing whitespace tolerant) | `src/tools/fs_utils.rs` |
| Repetitive token loops | `repeat_penalty=1.1` | `src/config.rs` |
| Over-explaining prose before tool calls | Strict output rule in prompt | `src/llm/prompt.rs` |
| Context corruption from tool chatter | Hard truncation + write-aware elision + tool-result compressor | `src/tools/mod.rs`, `src/agent/context.rs`, `src/agent/compress.rs` |
| Edit duplication in repetitive files | Unique-match default, `replace_all=true` opt-in | `src/tools/fs_write.rs` |
| File corruption on crash mid-write | Atomic writes (temp + fsync + rename) | `src/tools/fs_write.rs` |
| Shell tool entropy | shlex parse + metacharacter reject + allowlist | `src/tools/shell.rs` |
| Editing files the model has never read | Structural read-before-write enforcement | `src/agent/guards.rs` |
| Forgetting prior edits during elision | Write summaries preserved | `src/agent/context.rs` |
| Wall-of-text tool outputs | Compact rendering + `/last /tool N` for expansion | `src/repl/render.rs` |
| Routing entropy from too many tools | 7-tool lean surface, no `glob` | `src/main.rs:build_tool_surface` |
| Cold-starting `llama.cpp` on every reset | Singleton server, `/reset` clears conv only | `src/server.rs` |
| Model emits absolute `/src/...` for relative `src/...` | Leading-slash fallback in `safe_path` | `src/tools/fs_utils.rs` |
| `max_tokens` truncation produces incomplete tool_calls | Length-truncation exit + concision note | `src/agent/mod.rs`, `src/agent/guards.rs` |

Discovered during live smoke, not from prior analysis: the leading-slash row.
The model emitted `/src/` consistently; harness now strips the leading slash
if the absolute form would escape the cwd but the relative form is valid.

## Out of scope (v1)

Sub-agents, plan mode, MCP tools, tree-sitter symbols, BM25 search, the `glob` tool (routing entropy at 1.5 B), multi-file refactors, streaming output. See [`ARCHITECTURE.md §Non-goals`](ARCHITECTURE.md) for the rationale and the would-it-help analysis.

## Observability & benchmarking

`micro-mind` records every conversation it serves on demand. The schema is
small, stable, and additive so downstream tooling stays useful as the
harness evolves.

```bash
# Record this REPL session to JSONL.
cargo run --release -- --record obs/runs

# Drive the bench fixtures, write per-task traces + summary.json.
cargo run --release --bin bench-run -- --reps 3 --out bench/runs/today

# Aggregate any directory of traces into a markdown table.
cargo run --bin bench-summarize -- --md bench/runs/today/

# Validate traces against fixtures *without* the model (CI-friendly).
cargo run --bin bench-replay -- --all bench/tasks --runs bench/runs/today

# Diff against a checked-in baseline; non-zero exit on regression.
cargo run --bin bench-compare -- \
  --baseline bench/baselines/<date>-<label>/summary.json \
  --candidate bench/runs/today/summary.json
```

Baselines live as directories (`bench/baselines/<date>-<label>/`) containing
both the aggregate `summary.json` and per-rep JSONL traces. CI replays every
committed baseline against the current fixture set as an advisory check.

Documentation:
- [`obs/schema.md`](obs/schema.md) — JSONL event schema (envelope, variants, `jq` recipes).
- [`bench/README.md`](bench/README.md) — fixture format, the four bench binaries, CI integration.
- [`bench/ablations.md`](bench/ablations.md) — sketched experiments (KV cache, tool surface, semantic summaries).
- [`bench/baselines/README.md`](bench/baselines/README.md) — how baselines are captured, named, and retired.

CI (`.github/workflows/ci.yml`) runs `cargo test`, `cargo fmt --check`,
`cargo clippy -D warnings` (gating), and the schema/replay/summarize
binaries against the checked-in sample trace plus every committed
baseline. **It does not invoke the model** — that requires `llama-server`
plus a GPU and is not deterministic across runners.

## Publishing claims responsibly

When citing numbers from this rig (latency, token count, pass rate):

- Always include the commit hash, fixture set, and `--reps`.
- Latency comes from a `--release` build with a warmed `llama-server` (one
  throwaway prompt before measurement). Debug builds are noise.
- Pass-rate numbers are over the fixture set — they reflect what *we
  chose to test*, not "the model's accuracy." Don't extrapolate to BFCL
  / HumanEval / etc. without re-benching there directly.
- `temperature=0.0, seed=42` are pinned. If you change them, say so —
  the lead from `neo-llm-bench` (2026-05-14) doesn't survive
  `temperature=0.7`.

## Documentation

- [`README.md`](README.md) — this file
- [`CLAUDE.md`](CLAUDE.md) — orientation for AI agents working on this codebase
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — module map, runtime flow, mitigations detail
- [`agents.md`](agents.md) — the single-agent spec (system prompt, tool surface, loop)
- [`lessons.md`](lessons.md) — running log of mistakes and hard-won insights
- [`obs/schema.md`](obs/schema.md) — recorded-event schema
- [`bench/README.md`](bench/README.md) — benchmarking workflow

## License

(unspecified)
