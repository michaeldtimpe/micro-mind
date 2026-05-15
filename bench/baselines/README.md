# Baselines

Checked-in baseline `summary.json` files used as the comparison target by
`bench-compare`.

## Naming

`<YYYY-MM-DD>-<short-label>.json` — e.g. `2026-05-15-initial.json`.

The date is the bench run, not the commit. Keep the label short
(`initial`, `post-coach-tuning`, `q8-kv`).

## How to add a new baseline

```bash
# 1. Run on a clean checkout with the model warm.
cargo run --release --bin bench-run -- --reps 3 --out bench/runs/baseline

# 2. Confirm all tasks pass.
cat bench/runs/baseline/summary.json | jq '.n_failures'

# 3. Copy the summary in with a date-stamped name.
cp bench/runs/baseline/summary.json bench/baselines/2026-05-15-initial.json

# 4. Commit it. Baselines are small (KB), checking them in is fine.
git add bench/baselines/2026-05-15-initial.json
git commit -m "bench: capture baseline 2026-05-15"
```

## How to retire a baseline

Don't delete — when a baseline becomes stale, just stop referencing it
and pick a fresher one. Older baselines remain useful for archaeology
(e.g. "when did the read_file p50 regress?").

## Format

The file is a JSON object produced by `bench-run`:

```json
{
  "schema_v": 1,
  "n_outcomes": N,
  "n_failures": 0,
  "outcomes": [ { "id": "...", "passed": true, "stats": { ... } }, ... ]
}
```

`bench-compare` reads `.outcomes[].id`, `.passed`, and `.stats.wall_ms` /
`.stats.total_tokens`. Other fields are ignored, so a future bump in the
`Summary` struct stays compatible as long as those four are present.
