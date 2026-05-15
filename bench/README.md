# bench/

Scaffold for the upcoming benchmarking work. **Phase 2+** — intentionally
not built out yet. The Phase 0/1 changes in `obs/` and the harness now emit
enough structured data for a downstream rig to consume; that rig lives here
when it's ready.

## Current status (Phase 1 complete)

- `--record <dir>` writes a JSONL stream of every chat round-trip, tool call,
  guard, and stop reason. See `obs/schema.md`.
- `Usage` from llama-server is captured per response and surfaced on `/explain`.
- The harness no longer hard-codes a developer-only `llama-server` path
  (env var → `PATH` → fallback).

## Phase 2 (next, not yet started)

A small in-repo bench runner. Sketch:

```
bench/
├─ README.md                this file
├─ tasks/                   fixture prompts + expected pass criteria (one TOML per task)
├─ run.rs                   binary: drive micro-mind on each task, collect JSONL
└─ summarize.rs             read JSONL, compute per-task pass/fail + cost metrics
```

Acceptance criteria for Phase 2 will be:

1. Reproducible (`temperature=0.0`, `seed=42` are already pinned in `config.rs`).
2. CI-friendly: exits non-zero on regression, prints a single-line summary.
3. Emits an artifact that `neo-llm-bench` can compare against the
   2026-05-14 baseline without translation.

Until then: collect data with `--record`, post-process with `jq`, do not
add benchmark code under this directory speculatively.

## Running with recording today

```bash
mkdir -p obs/runs
cargo run --release -- --record obs/runs
# in another shell, summarize after exit:
jq -c 'select(.payload.event=="chat_response") | .payload' obs/runs/*.jsonl
```
