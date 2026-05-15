# Observability event schema (v2)

micro-mind emits append-only JSONL when run with `--record <dir>`:

```
cargo run -- --record ./obs/runs
# → writes ./obs/runs/micro-mind-<unix_ms>.jsonl
```

One JSON object per line. Lines are independent — safe to `tail -f`, `jq`, or
shard across cores. The schema is intentionally small and stable so downstream
tools (notebooks, `neo-llm-bench` post-processors) don't need to track changes.

## Envelope

Every line has the same envelope:

```json
{
  "ts_ms": 1747312345678,
  "payload": { "event": "<type>", ... }
}
```

- `ts_ms` — Unix epoch milliseconds when the event was queued (host clock).
- `payload.event` — discriminator. One of the variants below.

## Event variants

### `session_start`

Emitted once at startup when recording is enabled.

```json
{"event":"session_start","cwd":"/Users/m/proj","model":"qwen25-1.5b-instruct","tools":["read_file","grep","..."],"schema_v":2}
```

`schema_v` is optional (omitted by v1 emitters); when absent, readers should
assume v1. v1 traces remain forward-compatible: every v2 field is optional.

### `chat_request`

Emitted just before the POST to `/v1/chat/completions`.

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | 0-indexed turn within `run_turn` |
| `n_messages` | usize | conversation length sent |
| `n_tools` | usize | tools advertised to the model |

### `chat_response`

Emitted after the response is decoded (or after the request errors).

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | matches the request |
| `wall_ms` | u64 | request RTT incl. decode |
| `finish_reason` | string? | `"stop"`, `"tool_calls"`, `"length"`, … |
| `native_tool_calls` | usize | count returned by server in `tool_calls` |
| `recovered_tool_calls` | usize | tool calls parsed out of prose (text-channel fallback) |
| `prompt_tokens` | u32? | from `usage.prompt_tokens` |
| `completion_tokens` | u32? | from `usage.completion_tokens` |
| `total_tokens` | u32? | from `usage.total_tokens` |
| `error` | string? | populated if the HTTP call failed |

`*_tokens` are `null` when llama-server did not report `usage`. Treat
`native_tool_calls + recovered_tool_calls > 0` as "this turn took a tool path".

### `tool_call`

Emitted just before a tool function is invoked. **Guard interceptions
(`dedup`, `read_before_write`) do not emit this event** — they emit `guard`
instead.

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | |
| `name` | string | tool name (whitespace-trimmed) |
| `arguments` | object | the parsed JSON args |
| `tool_call_id` | string | echoes the assistant message's `tool_calls[].id` |

### `tool_result`

Emitted after dispatch finishes (incl. unknown-tool / validation errors).

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | |
| `name` | string | |
| `tool_call_id` | string | |
| `ok` | bool | `error.is_none()` |
| `wall_ms` | u64 | dispatch wall time |
| `bytes_out` | usize | size of the post-truncation body sent to the model |
| `cached` | bool | served from `ToolCache` |
| `error` | string? | tool-layer error message if any |

### `guard`

Harness guard fired (the loop didn't dispatch / kept dispatching against intent).

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | |
| `kind` | string | `dedup`, `read_before_write`, `write_pressure`, `turn_cap` |
| `detail` | string? | path, tool name, or other context |

### `stop`

Last event of a `run_turn`. The loop will not emit further events until the
next user input.

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | turn index at termination |
| `reason` | string | one of `FinalAnswer`, `TurnCap`, `WritePressure`, `Dedup`, `Length`, `Error: …` |
| `wall_ms` | u64 | total elapsed from start of `run_turn` |
| `final_answer` | string? | (v2) most recent non-empty assistant content, if any. For `FinalAnswer` stops this is the user-visible answer; for other terminations it's a best-effort snapshot of the model's last prose. Omitted when no assistant message produced visible content. |

## Final assistant content

Schema v2 carries the most recent assistant prose in `stop.final_answer`,
so `bench-replay` (offline, trace-only) can validate
`expect.must_contain` without needing a subprocess capture. Pre-v2 traces
omit the field and will fail-closed on that predicate unless `bench-run`
fills it from the subprocess's stdout.

Privacy/size tradeoff: traces now include verbatim model output. If that's
a problem for a particular session, scrub the stop events post-hoc.

## Stability guarantees

- New optional fields may be added without bumping the schema version.
- New `event` variants may be added; consumers must tolerate unknown variants.
- Existing field names and types will not change without a `schema_v` bump.

## Quick recipes

Total tokens spent in a session:
```bash
jq -s '[.[] | select(.payload.event=="chat_response") | .payload.total_tokens // 0] | add' run.jsonl
```

Per-tool wall-time histogram:
```bash
jq -r 'select(.payload.event=="tool_result") | "\(.payload.name)\t\(.payload.wall_ms)"' run.jsonl
```

Recovered-vs-native tool call ratio (proxy for prose-channel leakage):
```bash
jq -s '
  [.[] | select(.payload.event=="chat_response")]
  | {native: (map(.payload.native_tool_calls) | add),
     recovered: (map(.payload.recovered_tool_calls) | add)}
' run.jsonl
```
