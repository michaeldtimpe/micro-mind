# bench/

End-to-end benchmarking workflow for `micro-mind`. Five phases — all
currently implemented at MVP fidelity, deliberately under-built so each
piece stays easy to read.

```
bench/
├── README.md            ← you are here
├── tasks/               ← TOML fixtures, one per task (12 today)
│   ├── 01-read-readme.toml
│   ├── 02-list-rust-files.toml
│   ├── 03-decline-irrelevant.toml
│   ├── 04-length-truncation.toml
│   ├── 05-write-from-scratch.toml
│   ├── 06-edit-file.toml
│   ├── 07-grep-many-matches.toml
│   ├── 08-bash.toml
│   ├── 09-dedup-untriggered.toml
│   ├── 10-write-pressure-untriggered.toml
│   ├── 11-write-file-placeholder.toml
│   └── 12-edit-file-read-or-write.toml
├── runs/                ← outputs land here, one subdir per run (gitignored)
├── samples/             ← checked-in reference trace + fixture for CI
│   ├── sample-trace.jsonl
│   └── sample-fixture.toml
├── baselines/           ← reference summaries for regression detection
│   ├── README.md
│   ├── main/            ← canonical, gating
│   └── archive/         ← historical, advisory
└── ablations.md         ← experimental knobs: KV cache, tool surface, summaries
```

The `*-untriggered` fixtures pin the *non-firing* behavior of guards
that are structurally unreachable on `qwen25-1.5b-instruct` at temp=0
(`SemanticDedup`, `WritePressure`). They're regression anchors — if a
future model swap or system-prompt rev starts triggering the guard,
the predicate flip catches it.

Two *positive* guard-fire fixtures complement the anchors:
- `11-write-file-placeholder` — `write_file`'s placeholder-rejection
  guard fires deterministically; recovery (9/10) and runaway-loop-into-
  Dedup (1/10) are both correct outcomes, predicates accept both. See
  `lessons.md` 2026-05-16.
- `12-edit-file-read-or-write` — `read_before_write` for `edit_file`
  fires deterministically; partial-recovery shape (1 tool_call,
  `read_file`, model delegates the second hop back to the user) is
  locked as the regression anchor. See `lessons.md` 2026-05-17 (both
  entries — the initial 0/3 recovery finding was superseded the same
  day by wiring `coach::guard_failure_memory_note` into the guard
  path).

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
  --baseline bench/baselines/main/summary.json \
  --candidate bench/runs/<ts>/summary.json \
  --md /tmp/delta.md
```

## Fixture format (`tasks/*.toml`)

```toml
id = "01-read-readme"
description = "Single-hop read of the project README."
prompt = "Read README.md and tell me in one sentence what micro-mind is."

# Optional. When true, bench-run spawns micro-mind in a fresh per-rep
# tempdir under bench/runs/.scratch-<id>-rep<N>/. Required for fixtures
# that exercise mutating tools so state doesn't leak across reps.
cwd_isolated = false

# Optional, honored only when cwd_isolated = true. Files written into
# the scratch dir before micro-mind starts. Use for edit_file fixtures
# (the file must exist before the model can read-then-edit it).
[[seed_files]]
path     = "story.txt"
content  = "The quick brown fox jumps over the lazy dog.\n"

# Optional, honored only when cwd_isolated = true. Empty directories
# created in the scratch dir, processed after seed_files. Needed when a
# fixture requires a truly-empty subdir (e.g. list_dir returning 0
# bytes); a seeded file would populate the dir.
seed_dirs = ["empty1", "empty2"]

[expect]
stop_reason          = "FinalAnswer"  # FinalAnswer | TurnCap | WritePressure | Dedup | Length | Error
min_tool_calls       = 1
max_tool_calls       = 4
must_call_any_of     = ["read_file", "grep"]   # at least one of these must be called
must_call_all_of     = ["read_file", "edit_file"]  # every entry must be called at least once
must_not_call        = ["write_file", "edit_file"]
min_tool_errors      = 1               # at-least-N tool results with ok=false
max_tool_errors      = 2               # at-most-N — bounds recovery-loop behavior
must_fire_guards     = ["cold_read"]   # every entry must fire at least once (kind axis)
must_not_fire_guards = ["read_before_write"]   # symmetric — none of these may fire
min_guard_fires      = 1               # total count across all kinds (count axis)
max_guard_fires      = 1               # orthogonal to must_fire_guards
max_wall_ms          = 60000
max_total_tokens     = 4096

[expect.must_contain]
text                 = "agent"        # substring in the final answer
case_insensitive     = true           # default true
```

All `expect.*` fields are optional. Empty `[expect]` = "anything goes,
just record a trace."

**Guard-fire predicate orthogonality.** Kind predicates
(`must_fire_guards` / `must_not_fire_guards`) answer "which guard
kinds fired?"; count predicates (`min_guard_fires` /
`max_guard_fires`) answer "how many total fires occurred?". They are
checked independently — a kind mismatch and a count mismatch each
generate their own failure message. Diagnostics enumerate fired kinds
from a `BTreeSet`, so when a `must_fire_guards` expectation fails the
"seen" list in the message is sorted and stable across runs. See
`src/bench/summary.rs::check_expectations`. Currently observed kinds:
`cold_read`, `read_before_write`, `length`, `dedup`, `write_pressure`,
`turn_cap`. New kinds added to `src/agent/guards.rs` will surface
automatically — no schema change required.

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

1. `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --all-targets` — all gating.
2. `bench-replay --schema-only` on `bench/samples/sample-trace.jsonl` to
   verify schema parser tolerance.
3. `bench-replay --fixture … --trace …` on the sample to verify the full
   predicate path.
4. `bench-summarize --md` to verify the markdown writer.
5. **`bench-replay --all bench/tasks --runs bench/baselines/main` — the
   canonical baseline gate.** Every committed trace must satisfy every
   fixture predicate. Currently 12 fixtures × 3 reps = 36/36.
6. Loop over `bench/baselines/archive/*/` and replay each advisorily
   (`continue-on-error: true`). Historical drift surfaces in logs but
   doesn't gate.

The model itself is never invoked in CI — it's too heavy and not
deterministic across runners. Model-in-the-loop runs happen on demand
locally; CI guarantees the *tooling around* those runs keeps working,
plus the canonical baseline traces stay consistent with current
fixture predicates.

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
  `bench/baselines/{main,archive/*}/` directories (each containing the
  per-rep JSONL traces + `summary.json`) are the long-lived artifacts.
- No statistical-test framework. With temp=0 and a deterministic seed,
  the per-task results should be stable enough that simple thresholds
  in `bench-compare` are sufficient.

See `bench/ablations.md` for the rough sketch of larger experiments.
