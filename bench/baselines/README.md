# Baselines

Checked-in bench runs that downstream tools compare against. Two tiers:

- `bench/baselines/main/` — **canonical, gating**. CI replays it on every
  push; any predicate regression fails the build. Replace it when the
  harness or fixtures change in a way that shifts measured behaviour.
- `bench/baselines/archive/<YYYY-MM-DD>-<short-label>/` — historical
  reference. CI replays these advisorily; failures are surfaced but don't
  gate. Older entries here are useful for archaeology ("when did the
  read_file p50 regress?") and as `bench-compare` deltas.

Each subdirectory carries the aggregate `summary.json` (consumed by
`bench-compare`) plus the per-rep JSONL traces (consumed by `bench-replay`).

## Layout

```
bench/baselines/
├─ main/                              # canonical, gated by CI
│  ├─ summary.json
│  ├─ <task-id>-rep0.jsonl
│  ├─ <task-id>-rep1.jsonl
│  └─ ...
└─ archive/                           # historical, advisory in CI
   ├─ 2026-05-15-main/
   │  ├─ summary.json
   │  └─ ...
   └─ 2026-05-15-with-length/
      └─ ...
```

## Replacing `bench/baselines/main/`

```bash
# 1. Build a release binary so timing data is meaningful.
cargo build --release --bin micro-mind --bin bench-run

# 2. Spawn llama-server yourself and export LLAMA_SERVER_URL so the
#    invocations share one warm server (~10× faster than letting each
#    bench-run subprocess cold-start).
~/code/llama.cpp/build/bin/llama-server -m ~/models/Qwen2.5-1.5B-Instruct-Q8_0.gguf \
    --ctx-size 8192 --n-gpu-layers 99 --threads 6 --batch-size 512 --ubatch-size 512 \
    --cache-type-k q8_0 --cache-type-v q8_0 --jinja --port 8080 &

# 3. Capture. Output goes straight into the baseline dir, replacing what
#    was there.
rm -rf bench/baselines/main
LLAMA_SERVER_URL=http://127.0.0.1:8080 \
    ./target/release/bench-run \
    --bin ./target/release/micro-mind \
    --reps 3 \
    --out bench/baselines/main

# 4. Optional: stash the prior baseline under archive/ before replacement
#    if it captured a notable configuration you might want to diff against
#    later. Use `git log -- bench/baselines/main/` to find the commit and
#    `git checkout <sha> -- bench/baselines/main` into archive/<date>-<label>.

# 5. Verify locally — the replay must pass since CI gates on it.
cargo run --bin bench-replay -- --all bench/tasks --runs bench/baselines/main

# 6. Commit. Baselines are small (~30 KB for 15 traces).
git add bench/baselines/main
git commit -m "bench: capture baseline (date / context)"
```

## Pass-rate policy

`bench/baselines/main/` must be 100 % passing — that's the contract CI
enforces. If you can't make it pass:

- Either calibrate the fixture predicate to the measured floor (see
  `lessons.md` for what counts as calibration vs capitulation), or
- Improve the harness so the predicate is reachable, or
- Move the prior baseline to `archive/` and commit a new one where the
  failure is genuinely tolerated and the fixture's `must_*` predicates
  reflect what's currently achievable.

Don't loosen `must_contain` or `must_not_call` to make things pass —
those are correctness assertions and weakening them dilutes the bench.
Loosen latency/token budgets when the architectural floor moves.

## CI usage

`.github/workflows/ci.yml` has two steps:

- **Replay canonical baseline (gating)** — `bench-replay --all bench/tasks
  --runs bench/baselines/main`. Fails the build on any predicate miss.
- **Replay archive baselines (advisory)** — loops over
  `bench/baselines/archive/*/` and replays each. `continue-on-error: true`
  so historical drift surfaces but doesn't gate.

`.github/workflows/bench-compare.yml` is a separate manually-dispatched
workflow that runs `bench-compare` between any two committed summaries.
Useful for "diff main against an old archive" or "diff a candidate branch
against main." The runner does not invoke the model.

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
