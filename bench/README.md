# bench/

End-to-end benchmarking workflow for `micro-mind`. Five phases — all
currently implemented at MVP fidelity, deliberately under-built so each
piece stays easy to read.

```
bench/
├── README.md            ← you are here
├── tasks/               ← TOML fixtures, one per task
│   ├── 01-read-readme.toml
│   ├── 02-list-rust-files.toml
│   └── 03-decline-irrelevant.toml
├── runs/                ← outputs land here, one subdir per run (gitignored)
├── samples/             ← checked-in reference trace + fixture for CI
│   ├── sample-trace.jsonl
│   └── sample-fixture.toml
├── baselines/           ← reference summaries for regression detection
│   └── README.md
└── ablations.md         ← experimental knobs: KV cache, tool surface, summaries
```

## Quick start

```bash
# 1. One-off run (auto-timestamped output dir).
cargo run --release --bin bench-run

# 2. Filter to a single task, 3 repetitions.
cargo run --release --bin bench-run -- --filter 01 --reps 3

# 3. Look at the results.
cargo run --bin bench-summarize -- --md bench/runs/<ts>/

# 4. Validate without the model (CI-friendly).
cargo run --bin bench-replay -- --all bench/tasks --runs bench/runs/<ts>

# 5. Diff against a baseline.
cargo run --bin bench-compare -- \
  --baseline bench/baselines/2026-05-15.json \
  --candidate bench/runs/<ts>/summary.json \
  --md /tmp/delta.md
```

## Fixture format (`tasks/*.toml`)

```toml
id = "01-read-readme"
description = "Single-hop read of the project README."
prompt = "Read README.md and tell me in one sentence what micro-mind is."

[expect]
stop_reason       = "FinalAnswer"     # FinalAnswer | TurnCap | WritePressure | Dedup | Error
min_tool_calls    = 1
max_tool_calls    = 4
must_call_any_of  = ["read_file", "grep"]
must_not_call     = ["write_file", "edit_file"]
max_wall_ms       = 60000
max_total_tokens  = 4096

[expect.must_contain]
text              = "agent"           # substring in the final answer
case_insensitive  = true              # default true
```

All `expect.*` fields are optional. Empty `[expect]` = "anything goes,
just record a trace."

## Binaries

| Binary | What it does | Needs llama-server? |
|---|---|---|
| `micro-mind` | Main REPL. `--record <dir>` writes JSONL. | yes |
| `bench-run` | Spawns `micro-mind` per fixture × reps, writes traces, runs predicate checks, emits `summary.json`. | yes |
| `bench-summarize` | Reads one or more JSONL traces, prints text/markdown/JSON aggregate. | no |
| `bench-replay` | Re-checks a JSONL trace against a fixture without re-running the model. CI-friendly. | no |
| `bench-compare` | Diffs candidate `summary.json` against a baseline. Exits non-zero on regression. | no |

## What "pass" means

A run *passes* a fixture iff every populated `expect.*` predicate is
satisfied by the trace summary. See `src/bench/summary.rs::check_expectations`.

## How CI uses this

The GitHub Actions workflow at `.github/workflows/ci.yml`:

1. Builds + runs `cargo test --all-targets`.
2. `bench-replay --schema-only` on `bench/samples/sample-trace.jsonl` to
   verify schema parser tolerance.
3. `bench-replay --fixture … --trace …` on the sample to verify the full
   predicate path.
4. `bench-summarize --md` to verify the markdown writer.

The model itself is never invoked in CI — it's too heavy and not
deterministic across runners. Model-in-the-loop runs happen on demand
locally; CI guarantees the *tooling around* those runs keeps working.

## Reproducibility checklist

- Pin `temperature=0.0`, `top_p=1.0`, `seed=42`. (Already enforced in
  `src/config.rs`.)
- Always run with `--release` for latency numbers — debug builds are
  noise.
- Warm the server first: do one throwaway prompt before measurement, so
  KV cache / model load isn't measured.
- Record at least 3 reps per task. The runner doesn't compute variance
  yet, but `bench-summarize --json` gives you the raw rows.

## Out of scope (deliberately)

- No web UI. `--md` output piped to `glow` / pasted into a PR is fine.
- No persistent results DB. Filesystem is the storage layer; checked-in
  `bench/baselines/*.json` are the long-lived artifacts.
- No statistical-test framework. With temp=0 and a deterministic seed,
  the per-task results should be stable enough that simple thresholds
  in `bench-compare` are sufficient.

See `bench/ablations.md` for the rough sketch of larger experiments.
