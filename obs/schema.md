# Observability event schema (v3)

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
{"event":"session_start","cwd":"/Users/m/proj","model":"qwen25-1.5b-instruct","tools":["read_file","grep","..."],"schema_v":3}
```

`schema_v` is optional (omitted by v1 emitters); when absent, readers should
assume v1. v1 and v2 traces remain forward-compatible: every v2 and v3 field
is optional with a documented default.

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

Emitted just before a tool function is invoked. **Guard interceptions that
end the iteration without dispatching (`dedup`) do not emit this event** —
they emit `guard` only. The `read_before_write` guard *does* emit a
`tool_call` / `tool_result` pair when the harness auto-reads the target on
the model's behalf (v3); the pair carries `origin.kind =
"synthetic_guard_recovery"` so consumers can distinguish it from model
output.

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | |
| `name` | string | tool name (whitespace-trimmed) |
| `arguments` | object | the parsed JSON args |
| `tool_call_id` | string | echoes the assistant message's `tool_calls[].id` |
| `origin` | object? | (v3) provenance — see [Tool provenance](#tool-provenance). Omitted when the call originated from the model (the common path). |

### `tool_result`

Emitted after dispatch finishes (incl. unknown-tool / validation errors).
Synthetic recoveries (v3) emit this event too, with `origin` set to mark
the harness-injected path.

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
| `origin` | object? | (v3) provenance — see [Tool provenance](#tool-provenance). Omitted when model-originated. |

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

## Tool provenance

Schema v3 distinguishes model-originated tool calls from harness-injected
ones via the optional `origin` field on `tool_call` and `tool_result`
events. Two variants are defined:

**Model-originated** (the common path): `origin` is omitted from the wire.
Pre-v3 traces all fall into this bucket — consumers MUST treat a missing
`origin` field as model-originated, never as "unknown."

**Synthetic guard recovery**: harness fabricated this call as part of
guard-driven recovery. Today this is only emitted by the `read_before_write`
auto-read path (v3): when the guard refuses an `edit_file`/`write_file`
against an unread target, the harness performs a bounded `read_file` itself
and feeds the content into the conversation, collapsing what would be a
two-hop model chain (refusal → model retries with read → model reads →
model retries with edit) into a single hop the model can actually sustain
on the 0% BFCL multi-turn floor (see `lessons.md` 2026-05-17).

Wire form when present:

```json
"origin": {"kind": "synthetic_guard_recovery", "guard": "read_before_write"}
```

The `guard` field is required for the `synthetic_guard_recovery` variant
and names the guard kind that triggered the auto-recovery. New synthetic
sources opt in by adding their variant here.

### Replay invariants

- Bench predicates that don't assert provenance MUST behave identically
  across v1/v2/v3 traces. Adding `origin` is purely additive.
- The `must_have_synthetic_calls` / `must_not_have_synthetic_calls`
  predicates on a fixture's `[expect]` table fail-closed when run against
  a pre-v3 trace whose `origin` is uniformly omitted: predicates that
  positively assert synthetic-call presence cannot be satisfied by a v1/v2
  emitter. This is intentional — exercising the auto-read contract
  requires a v3-emitting runtime.
- The synthetic-recovery `tool_call` / `tool_result` events appear in
  trace order *after* the corresponding `guard` event. Counterfactual
  visibility: the refusal that motivated the auto-read is still present
  in the trace, not silently rewritten.

### Schema-migration compatibility surface

A separate, narrower contract than the predicate-replay invariants above.
Pinned by the `bench-replay --migration-check <dir>` CI step (gating):
any committed v1/v2/v3 trace must (a) parse without errors and (b)
produce a `Summary` whose pinned fields are present, well-typed, and
internally consistent.

**Pinned** (CI fails if these break):

| Field | Why |
|---|---|
| Trace parses to `Vec<TraceEvent>` | Deserialization-compat baseline |
| `Summary.tool_calls` | Predicate-input contract |
| `Summary.tool_errors` | Predicate-input contract |
| `Summary.tool_calls_by_name` | Per-tool predicate inputs |
| `Summary.guards_by_kind` + `Summary.guard_fires` | Guard predicate inputs |
| `Summary.synthetic_tool_calls` + `Summary.synthetic_tool_calls_by_name` | Provenance predicate inputs (schema v3) |
| `Summary.model_tool_calls` (derived) | Compositionality predicate input (schema v3) |
| Accounting identity: `model_tool_calls + synthetic_tool_calls == tool_calls` | Catches `summarize_trace` regressions that forget to derive `model_tool_calls` consistently |

**Not pinned** (drift allowed across schema versions):

| Field | Why not |
|---|---|
| `Summary.total_tokens`, `Summary.prompt_tokens`, `Summary.completion_tokens` | Drift expected across server-state / model rev |
| `Summary.wall_ms` | Drift expected across rig + cache state |
| `Summary.final_answer` | Best-effort across v1 (absent) / v2 / v3 — explicitly tolerated as `None` for pre-v2 traces |
| `Summary.stop_reason` | Stop reason set changes over time (e.g., `Length` added in v2-era harness change) |

Adding a field to "not pinned" is a schema-erosion risk. If a future
predicate ever depends on one of those fields, promote it to "pinned"
in the same commit and capture the rationale here.

The migration check is hermetic — no fixture matching, no predicate
evaluation against expectations, no model. It exclusively tests the
deserialization layer. The advisory archive replay (`Replay archive
baselines` in CI) is the separate semantic-replay test and is allowed
to fail because fixture predicates have moved; the migration check is
not.

## Stability guarantees

- New optional fields may be added without bumping the schema version.
- New `event` variants may be added; consumers must tolerate unknown variants.
- Existing field names and types will not change without a `schema_v` bump.
- Any change to the pinned fields above requires either a `schema_v` bump
  or a corresponding update to the migration check's accounting invariants.

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
