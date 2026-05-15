# Baselines

Checked-in historical bench runs. Each subdirectory is one captured run,
containing both the aggregate `summary.json` (consumed by `bench-compare`)
and the per-rep JSONL traces (consumed by `bench-replay` in CI).

## Layout

```
bench/baselines/<YYYY-MM-DD>-<short-label>/
├─ summary.json                      # bench-compare input
├─ <task-id>-rep0.jsonl              # per-rep trace
├─ <task-id>-rep1.jsonl
└─ ...
```

The date is the bench run, not the commit. Keep the label short
(`main`, `post-coach-tuning`, `q8-kv`).

## How to add a new baseline

```bash
# 1. Build a release binary so timing data is meaningful.
cargo build --release --bin micro-mind --bin bench-run

# 2. Optional but recommended: spawn llama-server yourself and export
#    LLAMA_SERVER_URL so the 9 invocations share one warm server (~9× faster
#    than letting each bench-run subprocess cold-start).
~/code/llama.cpp/build/bin/llama-server -m ~/models/Qwen2.5-1.5B-Instruct-Q8_0.gguf \
    --ctx-size 8192 --n-gpu-layers 99 --threads 6 --batch-size 512 --ubatch-size 512 \
    --cache-type-k q8_0 --cache-type-v q8_0 --jinja --port 8080 &

# 3. Run the bench. Output goes straight into the baseline dir.
LLAMA_SERVER_URL=http://127.0.0.1:8080 \
    ./target/release/bench-run \
    --bin ./target/release/micro-mind \
    --reps 3 \
    --out bench/baselines/2026-05-15-main

# 4. Commit. Baselines are small (~20 KB for 9 traces).
git add bench/baselines/2026-05-15-main
git commit -m "bench: capture baseline 2026-05-15-main"
```

Pass rate is NOT a prerequisite for committing. The baseline is the
*current* behavior, warts and all — failures are tracked aspirations.
Improvements should show up as failures flipping to passes on the next
baseline.

## How to retire a baseline

Don't delete — when a baseline becomes stale, just stop referencing it
and pick a fresher one. Older baselines remain useful for archaeology
(e.g. "when did the read_file p50 regress?").

## CI usage

`.github/workflows/ci.yml` replays every `bench/baselines/*/` directory
against the current fixture set. Advisory for now (`continue-on-error: true`)
so commits don't get blocked by a model-side regression that has nothing to
do with the harness change being reviewed.

## summary.json format

```json
{
  "schema_v": 1,
  "n_outcomes": N,
  "n_failures": K,
  "outcomes": [ { "id": "...", "passed": true|false, "stats": { ... } }, ... ]
}
```

`bench-compare` reads `.outcomes[].id`, `.passed`, and `.stats.wall_ms` /
`.stats.total_tokens`. Other fields are ignored, so a future bump in the
`Summary` struct stays compatible as long as those four are present.
