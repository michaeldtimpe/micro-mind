# `bench/archive/` — frozen fixture shapes

This directory holds bench fixtures pinning *historical* trace shapes
that the harness no longer produces. They're outside `bench/tasks/` so
`bench-run` and CI don't discover them automatically.

## Why archive instead of delete

When a harness change collapses a multi-hop chain into a single hop (or
otherwise alters the deterministic shape a fixture used to pin), the
naive move is to update the existing fixture's predicates in place.
That works, but loses a useful signal: if a future change re-introduces
the old shape, no fixture in the canonical suite catches it.

Archive fixtures fill that gap. Each one is a **regression canary** —
it asserts the *old* shape, and the canonical fixture asserts the
*new* shape. In steady state, the canonical fixture passes and the
archive fixture fails. A change that flips both (canonical RED,
archive GREEN) is a regression.

## How to run a regression check

```bash
bench-run --tasks bench/archive \
          --filter <fixture-id> \
          --reps 3 \
          --out /tmp/regression-check
bench-replay --all bench/archive --runs /tmp/regression-check
```

If the replay **FAILS**: the new behavior is intact, archive fixture is
correctly anchoring the obsolete shape.

If the replay **PASSES**: the harness has regressed to the old shape —
investigate the change that caused it.

## Inventory

| Fixture | Pins | Closed by |
|---|---|---|
| `12-edit-file-read-or-write-pre-auto-read.toml` | First-hop-only recovery on `read_before_write` (model performs the recovery read, but stops before composing the edit). | Auto-read on guard refusal (option b), 2026-05-17 — see `lessons.md` and `src/agent/mod.rs::try_auto_read_for_rbw`. |
