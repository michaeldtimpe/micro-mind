# Ablation experiments

The bench infrastructure exists so we can answer questions like "did
removing the tool summary hurt accuracy?" The shape of each experiment is
the same:

1. Run `bench-run` with the unmodified harness → call this `baseline.json`.
2. Toggle one knob (env var, code change, fixture override).
3. Run `bench-run` again → `candidate.json`.
4. `bench-compare --baseline baseline.json --candidate candidate.json --md ablation.md`.

Below are the three experiments most worth doing first. None are
implemented yet — the harness only provides the *infrastructure* to run
them cleanly.

## 1. KV cache / prefix stability

**Question.** How much does llama-server's prompt cache buy us across
multi-turn traces?

**Knob.** Restart `llama-server` between every chat round-trip (forces
cache cold). Implementation hooks:

- Add `MICROMIND_DISABLE_PREFIX_CACHE=1` checked at server spawn time;
  pass `--prompt-cache-all 0` or equivalent llama-server flag.
- Alternatively, add `--no-cache` to `bench-run` that restarts the
  server after each task. Cleaner, more expensive.

**Predicted result.** Latency per `chat_response` event roughly 2–4×
worse on multi-turn tasks; tokens unchanged. If unchanged, our
multi-turn savings are not coming from KV reuse and that's worth
investigating.

**Measurement.** `bench-compare` will surface this automatically as a
`soft-regression (wall)` per task.

## 2. Tool-surface ablation

**Question.** Does removing rarely-used tools improve over-call resistance
on irrelevant prompts (like fixture `03-decline-irrelevant`)?

**Knob.** Conditionally exclude tools from `build_tool_surface()` in
`src/main.rs` based on an env var:

```rust
let mut tools = build_tool_surface(&cwd);
if let Ok(drop) = std::env::var("MICROMIND_DROP_TOOLS") {
    let names: Vec<_> = drop.split(',').collect();
    tools.retain(|t| !names.contains(&t.name.as_str()));
}
```

Run the bench with `MICROMIND_DROP_TOOLS=bash,write_file,edit_file` to
test "read-only mode" routing entropy.

**Predicted result.** `must_not_call` predicates should stay green;
`tool_calls` on math-style tasks should drop toward zero;
`stop_reason=FinalAnswer` rate on irrelevant prompts should rise.

**Open question.** Does removing tools hurt legitimate write tasks
enough to matter? Need separate fixtures with `min_tool_calls` on
`write_file`.

## 3. Semantic summary ablation

**Question.** Is the per-tool summary (`src/agent/compress.rs::summarize`)
load-bearing for multi-step tasks, or just noise?

**Knob.** Add `MICROMIND_DISABLE_SUMMARIES=1` to short-circuit
`compress::summarize` to `None`. Then run multi-hop fixtures (none in
the v1 set — add `04-multi-hop-grep-then-read` first).

**Predicted result.** Either:
- Removing summaries hurts → keep them, document why.
- Removing summaries doesn't hurt → drop them, save ~50 tokens / call.

Either outcome is publishable. The current state ("summaries are on
because that's what `luxe` did") is the one we want to avoid leaving in.

## Cross-cutting hooks already in place

- `--record` writes the JSONL we need for all three experiments.
- `chat_response` includes `prompt_tokens` and `total_tokens` per turn,
  so KV-cache savings show up at the token level too (cached prefix
  → same prompt_tokens, lower latency).
- `tool_result.cached` distinguishes harness-cache hits from
  llama-server cache hits — they live at different layers.

## What we should NOT build yet

- Multi-seed sweeps. `temperature=0.0, seed=42` is pinned; we'll
  introduce sweeps only when we have a concrete experiment that needs
  them.
- Statistical significance machinery. Three reps + a 20% wall threshold
  is enough signal until we have data that says otherwise.
- A nice UI. Markdown tables piped to a PR comment will do.
