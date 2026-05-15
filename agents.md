# Agent Reference

micro-mind runs a **single agent** — the same `qwen25-1.5b-instruct` model with a single fixed system prompt and the v1 tool surface. There is no architect / worker split, no per-task model selection, and no router. This document describes that one agent end-to-end.

The single-agent design is deliberate: `neo-llm-bench` measured a 0 % multi-turn floor for this model size class, so any orchestration on top of it burns context without buying capability. Compared to [`luxe`'s agent reference](https://github.com/michaeldtimpe/luxe/blob/main/agents.md), micro-mind is the degenerate case — one agent, one prompt, one model.

## Agent: micro-mind

**Purpose**: Drive a development-assistant REPL — read code, search, summarize, and make small targeted edits. Decline irrelevant requests in plain prose.

| Property | Value |
|---|---|
| Model | `qwen25-1.5b-instruct` (GGUF Q8_0, ~1.9 GB) |
| Runtime | `llama-server` (llama.cpp), Metal-offloaded |
| Context | 8,192 tokens |
| Max steps | 8 (`config::MAX_TURNS`) |
| Temperature | 0.0 (mandatory) |
| Top-p | 1.0 |
| Repeat penalty | 1.1 |
| Seed | 42 |
| Max tokens | 2,048 per assistant turn |
| KV cache | Q8_0 (k + v) |

**Tool surface** (`src/main.rs::build_tool_surface`):

`read_file`, `list_dir`, `list_files_recursive`, `grep`, `write_file`, `edit_file`, `bash`. See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the per-tool contracts, schemas, and caps.

## System prompt

Built by `src/llm/prompt.rs::system_prompt(cwd)`. Five blocks, kept under ~300 tokens. The contract:

```
You are micro-mind, a development assistant operating inside <CWD>.

Tool-use rules:
- To invoke a function on N inputs, emit N separate tool calls.
  Do not pack multiple inputs into array arguments.
- If the available tools cannot satisfy the user's request, do not
  call any tool — answer in plain text.
- Use Python operator syntax for math: `x**2`, `3*x`. Not `^`.

Behaviour:
- Prefer the smallest action that directly answers the user.
- If a tool call is required, emit it immediately. Do not apologize
  or narrate before the call. Explain only after the result is in,
  and only if the user benefits.
- Read a file before modifying it.
- After a successful write, verify with ONE concise read or test command,
  then stop. Do not continue searching once the answer is known.

Working directory: <CWD>
```

The first block ("Tool-use rules") is lifted verbatim from `neo-llm-bench`'s BFCL v2 system prompt — the exact text that produced the 77.1 % BFCL matched score on this model. The second block ("Behaviour") is original to micro-mind and addresses three known failure modes (agentic drift, over-explaining prose, blind edits) that the BFCL prompt didn't target.

**Pinned tests** (`src/llm/prompt.rs::tests`):

- `includes_anti_overcall_rule` — the "do not call any tool" line is present.
- `includes_parallel_rule` — the "N separate tool calls" line is present.
- `includes_read_before_write` — the read-before-modify line is present.
- `stays_compact` — total length under 1,500 chars (≈300 tokens).

Changes to the prompt must keep all four passing.

## Agent loop

Shared, single-purpose. Implemented in `src/agent/mod.rs::run_turn`:

```
push user message
loop (max MAX_TURNS=8 turns):
    if pressure > 0.7
        → elide_old_tool_results (write-summaries preserved)
    record ChatRequest event
    response = client.chat(messages, tools)
    record ChatResponse event (finish_reason, usage, tool_call counts)
    push assistant message; remember its content for final_answer
    if response.finish_reason == "length"
        → record Guard{length}, push concision note, stop=Length, break
    if response.tool_calls.is_empty()
        → render final answer, stop=FinalAnswer, break
    for each tool_call:
        if SemanticDedup catches a 3-in-a-row loop
            → inject system note, stop=Dedup, break
        if edit_file && target unread in this turn
            → return tool-failure stub ("read it first"), continue
        if write_file && target exists on disk && unread in this turn
            → return tool-failure stub ("survey first via list_dir"), continue
        if turn == 0 && read_file && user input doesn't mention this path
            → return tool-failure stub ("user didn't reference this"), continue
        record ToolCall event
        result = dispatch(name, args, …)
            (8 KB hard cap applied here)
        coached_body = coach::coach(&result)
        push tool-result message with coached_body
        record ToolResult event
        if let Some(summary) = compress::summarize(&result)
            → push as system note
        if result.error
            → push failure-memory system note
        if result.is_ok()
            → ReadTracker.record_read(name, args)
        if WritePressure trips (3 zero-byte non-writes after a write)
            → stop=WritePressure, break early
record Stop event (turn, reason, wall_ms, final_answer)
```

Stop reasons: `FinalAnswer`, `TurnCap`, `WritePressure`, `Dedup`, `Length`, `Error(String)`. All five appear as fixture-predicate strings in `bench/tasks/*.toml` and as `stop.reason` in JSONL traces.

Two response shapes are handled:

- **Native `tool_calls`** (preferred). `qwen25-1.5b-instruct`'s OpenAI-compatible template emits these reliably; the BFCL bake-off measured 99.5 % native-channel usage on this model.
- **Text recovery** (fallback). If `tool_calls` is empty but the assistant content contains `<tool_call>{...}</tool_call>` or a bare top-level JSON object with `name` + `arguments`, `src/llm/client.rs::recover_tool_calls_from_text` promotes it. Belt-and-braces for the rare miss.

Schema validation runs on every tool call before dispatch. A failed validation surfaces as a tool error and the model gets another turn to self-correct.

## Failure-mode coverage

This single agent inherits all 20 named mitigations described in [`ARCHITECTURE.md §Layered survival primitives`](ARCHITECTURE.md). The agent has no concept of these layers — it just sees: a strict prompt, tools that reject bad input, an early stop if it loops or gets truncated, refusals if it reads paths the user didn't mention or modifies paths it hasn't surveyed, and synthetic system notes when something goes wrong. The agent's job is just to call the right tool with the right args.

## Why not multiple agents?

Considered and rejected for v1:

| Pattern | Why not |
|---|---|
| Architect → Worker (luxe-style) | Multi-turn state-tracking is 0 % on this model. The architect's plan would be re-derived (badly) by the worker. |
| Planner emitting a tool-call DAG | Same as above. Single-action bias in the prompt does what a planner would, more reliably. |
| Reviewer agent for diffs | Reviewer pattern at 1.5 B is a coin flip; a deterministic linter is strictly better. |
| Sub-agent for shell commands | Adds a routing decision. Routing entropy is the primary failure axis at this size. |
| Tool-specific sub-agents (one for code, one for prose) | Doubles the surface; same routing problem. |

When the user's request genuinely doesn't fit on a 1.5 B model, the right answer is `luxe` (35 B MoE) — not "more agents around micro-mind".

## Related

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — the layered survival primitives + per-tool contracts.
- [`CLAUDE.md`](CLAUDE.md) — orientation for AI agents working on this codebase.
- [`lessons.md`](lessons.md) — running log of mistakes and hard-won insights.
