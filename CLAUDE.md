# Claude Code instructions for micro-mind

Auto-loaded at session start. Points at the durable design contracts and the short list of project-specific gotchas.

## Single-model policy

**micro-mind pins exactly one model: `qwen25-1.5b-instruct`** (`~/models/Qwen2.5-1.5B-Instruct-Q8_0.gguf`, GGUF Q8_0, ~1.9 GB). Picked by [`neo-llm-bench`](https://github.com/michaeldtimpe/neo-llm-bench) on 2026-05-14 against the non-dominated quadrilateral. Lead survives matched-ID correction; decline weakness is steerable at the system layer. **Do not introduce model fan-out**: no per-task model selection, no router, no A/B against another model unless the user explicitly asks for a re-bench against `neo-llm-bench`'s finalists.

The champion path is locked end-to-end:

- **Runtime**: `llama.cpp` via `llama-server`. Metal offload. OpenAI-compatible HTTP API at `/v1/chat/completions`.
- **Quant**: Q8_0 weights, Q8_0 KV cache.
- **Sampling**: `temperature=0.0`, `top_p=1.0`, `repeat_penalty=1.1`, `seed=42`, `max_tokens=2048`. Deviating from these costs accuracy — see `lessons.md` (forthcoming) when it accumulates.

## Project shape

A Rust binary + a thin library facet. Entry point `src/main.rs`. Library
crate (`src/lib.rs`) exposes only the observability schema and the bench
helpers so the four `bench-*` bins can reuse them without dragging in
`agent/tools/repl`. Talks to `llama-server` over HTTP via `ureq` (blocking,
no tokio — the REPL is single-threaded and the 80 MB tokio runtime weight
isn't worth it on a tool meant to leave headroom for the 1.6 GB model).

Module map (read `ARCHITECTURE.md` for the long version):

```
src/
├─ main.rs                clap CLI, build_tool_surface, REPL bootstrap
├─ lib.rs                 library facet (pub mod bench, obs)
├─ config.rs              all numeric defaults (ctx, sampling, caps)
├─ server.rs              llama-server lifecycle (singleton)
├─ llm/                   chat client, types, system prompt
├─ tools/                 7-tool surface + dispatch + cache + fs_utils
├─ agent/                 run_turn loop + guards + context + compress + coach
├─ repl/                  rustyline UI + compact rendering
├─ obs/                   Recorder trait + JSONL recorder (schema: obs/schema.md)
├─ bench/                 Fixture/Summary types, trace parser, expectation checks
└─ bin/                   bench-run, bench-summarize, bench-replay, bench-compare

bench/
├─ tasks/                 *.toml fixtures (one task per file)
├─ baselines/             checked-in historical runs (dir-per-run, see README)
└─ samples/               sample trace + fixture exercised by CI
```

Observability is opt-in: pass `--record <dir>` to the main binary to append
JSONL events for the session. CI is hermetic — it exercises `bench-replay`
+ `bench-summarize` against `bench/samples/`, gates the build on the
canonical baseline at `bench/baselines/main/`, and runs an advisory replay
of every directory under `bench/baselines/archive/`. No llama-server
required at any CI step. `clippy -D warnings` and `cargo fmt --check`
both gate.

`bench/tasks/` fixtures can set `cwd_isolated = true` to run in a fresh
per-rep tempdir under `bench/runs/.scratch-<id>-rep<N>/`. Required for
mutating tools (`write_file`, `edit_file`) so reps don't leak state and
the project root stays clean. `bench-run` cleans the scratch dir on
success; keeps it on failure for inspection.

Fixtures can also declare `[[seed_files]]` entries (`path` + `content`).
Each is written into the scratch dir before `micro-mind` starts. Only
honored when `cwd_isolated = true`. Use this for `edit_file` tasks where
the file must exist before the model reads-and-edits it (see
`bench/tasks/06-edit-file.toml`).

For fixtures that need truly-empty subdirectories (e.g. baiting
`WritePressure` via `list_dir` on an empty dir), use the
`seed_dirs = ["a", "b/c"]` field. `seed_files` can't produce these on
its own — seeding `a/.gitkeep` populates `a/` with `.gitkeep`. `seed_dirs`
is processed after `seed_files`, so the two compose cleanly (see
`bench/tasks/10-write-pressure-untriggered.toml`).

## Architecture: layered survival primitives

The 1.5 B model can do **one** competent tool hop and decline irrelevant requests. It cannot reliably plan, recover from failure, or track state across many turns. Every architectural choice trades model capability for harness capability.

Three layers of mitigation:

1. **Prompt layer** (`src/llm/prompt.rs`) — BFCL v2 anti-over-call + parallel + math rules, plus single-action bias and strict-output behaviour.
2. **Tool layer** (`src/tools/`) — fuzzy edit-matching, atomic writes, honesty guards, shell metacharacter rejection, 8 KB hard output cap, large-file refusal.
3. **Agent loop layer** (`src/agent/`) — semantic dedup, read-before-write enforcement, write-pressure exit, length-truncation exit, failure-memory injection, write-aware context elision, per-tool semantic summarization, harness-level error coaching.

Stop reasons emitted by `run_turn` (all surfaced in `obs/schema.md` and as fixture predicates): `FinalAnswer`, `TurnCap`, `WritePressure`, `Dedup`, `Length`, `Error: …`.

Guards (in agent-loop order):
1. `turn_cap` — hit `MAX_TURNS=8` without resolution.
2. `dedup` — `SemanticDedup` fires (same normalized call 3 times in a row).
3. `read_before_write` — `write_file`/`edit_file` against an unread path. `write_file` only fires when the target already exists on disk; brand-new files skip the gate. **Auto-recovers** by synthesizing a bounded `read_file` (24 KB / 800 lines) with `origin=SyntheticGuardRecovery` provenance — the model's next turn observes the content already in scope and only needs to compose the original mutating call. Fallback to the pre-(b) refusal+memory shape if the auto-read errors or hits the line cap. (`src/agent/mod.rs::try_auto_read_for_rbw` 2026-05-17)
4. `cold_read` — `read_file` on turn 0 to a path the user's input didn't reference. Catches the BFCL "spurious tool call on self-answerable prompt" failure (e.g. `read_file(/dev/null)` on "What is 17+25?").
5. `length` — server reported `finish_reason="length"`; assistant message structurally incomplete.
6. `write_pressure` — successful write followed by `WRITE_PRESSURE_ZERO_BYTE_LIMIT=3` zero-byte non-write tool results.

When the model fails a task, the **first** question is "which layer should catch this?" Not "is the model dumb?". The 5 smoke workflows passed because of layered mitigations, not because the model is good.

## When working on this repo

1. **The harness is smarter than the model.** Don't add prompt-level requests when a tool-layer or agent-layer guard can do it deterministically. Prompt tokens are expensive (8192 ctx) and probabilistic; code is cheap and certain.
2. **Tools are mostly read-only.** `read_file`, `list_dir`, `list_files_recursive`, `grep`, `bash` are cached. Only `write_file` and `edit_file` mutate. Honesty guards live on those two.
3. **Every new tool costs routing entropy.** The 1.5 B model already over-calls on irrelevance (~57 %). Adding a tool means the model is choosing between N+1 options instead of N. Default to "no" on new tools; default to "add a flag to an existing tool" instead. We dropped `glob` from v1 for exactly this reason.
4. **`temp=0.0` is mandatory.** No exceptions. The `neo-llm-bench` t=0.7 swing is 10 pp on HumanEval; it will surface as fixture flakiness here too.
5. **`llama-server` is a singleton.** `/reset` clears the conversation, not the server. Don't add `restart-on-reset` — cold-start latency kills iteration.
6. **No async, no tokio.** `ureq` is the HTTP client. If you need parallelism for a future feature, the right answer is a separate worker thread, not a runtime.

## Bench / replay bins

| Bin | Purpose |
|---|---|
| `bench-run` | Drive `micro-mind` as a subprocess against every fixture in `bench/tasks/*.toml`, write per-rep JSONL traces + `summary.json`. Refuses fixtures whose prompt could short-circuit the REPL on stdin. Needs a working llama-server. |
| `bench-summarize` | Aggregate one or more JSONL traces into a text or markdown table (tools, tokens, wall_ms, stop). |
| `bench-replay` | Validate a trace against a fixture *without* running the model — the CI gate. Schema-only mode is also supported. |
| `bench-compare` | Diff a candidate `summary.json` against a baseline; exits 1 on outcome regression, 2 on soft regression (latency/tokens beyond `--wall-pct` / `--tokens-pct`). |

As of schema v2, the `stop` event carries `final_answer`, so `bench-replay` can fully validate `expect.must_contain` from a trace alone. Pre-v2 traces (no `schema_v` field on `session_start`) still fail-closed on that predicate unless bench-run's stdout capture fills it. See `obs/schema.md` for the version policy.

## Tool surface decisions

| Tool | Why this name / shape |
|---|---|
| `read_file` | `offset`+`max_bytes` mandatory. Default 24 KB, hard 64 KB. Anything bigger and the model loses the plot. Large-file refusal: >256 KB without offset → refuse and direct the model to `grep`. |
| `list_dir` | Non-recursive. Capped at 200 entries. |
| `list_files_recursive` | `.gitignore`-aware via the `ignore` crate. Depth-capped (3 default). 500-entry cap. **This is the project-map tool** — give the model orientation in one call instead of N `list_dir`s. |
| `grep` | Regex via the `regex` crate. Returns `file:line:match` lines. Cap 50 matches by default; 8 KB output cap. |
| `write_file` | Atomic (temp file + fsync + rename). Honesty guards: reject placeholders (`<your code here>`, `// TODO: fill in`, etc.) and mass deletion (>1 KB file → <100 B new content). |
| `edit_file` | **Fuzzy match by default** (CRLF + trailing whitespace tolerant). Unique-match enforced; `replace_all=true` is the explicit escape hatch. The single highest-value tool decision — exact-string-match `edit` is a loop generator on 1.5 B models. |
| `bash` | Allowlisted (~20 binaries). No pipes, redirects, `&&`, `;`, `$(...)`, backticks. No `-c`/`-e` flags for python/node (silent allowlist bypass). Output 8 KB cap. |

## Common task patterns

### "Add a new tool"

Don't, by default. Ask the user whether an existing tool can be extended instead. If a new tool is really warranted:

1. Define it in `src/tools/<area>.rs` returning `ToolDef` via `ToolDef::new(...)`.
2. Mark `.cacheable()` only if the call is referentially transparent (same args → same result modulo the filesystem).
3. Add to `build_tool_surface()` in `src/main.rs`.
4. Add a semantic-summary handler in `src/agent/compress.rs::summarize`.
5. Add an error-coach hint in `src/agent/coach.rs::hint_for_error` for the failure shapes you can predict.
6. Unit tests in the same file as the tool.

### "Change the system prompt"

`src/llm/prompt.rs::system_prompt`. Keep it under 1500 chars (≈300 tokens). Every token here costs one less token of tool-output budget at 8192 ctx. Pinned tests verify the BFCL v2 anti-over-call rule, the parallel rule, and the read-before-write line are present — keep them.

### "Wire up a new failure mode you saw in real use"

1. Reproduce in a smoke test (pipe a query into the binary).
2. Add a guard in `src/agent/guards.rs` if it's behavioural (dedup-style), a coach hint in `src/agent/coach.rs` if it's recoverable diagnostics, a tool-layer rejection in `src/tools/<area>.rs` if it's input validation.
3. If you add a new guard kind that fires via the `continue` branch in `src/agent/mod.rs` (rather than breaking the loop), audit whether the model would benefit from a "do not repeat" nudge — if yes, add a case to `coach::guard_failure_memory_note(tool, kind)` returning `Some` for the new kind. The call site is already generic; opt in per-kind. (Background: `read_before_write` originally had no failure-memory injection because the `continue` branch short-circuits before the dispatch path's `failure_memory_note` call — see `lessons.md` 2026-05-17.)
4. Update the README's "Survival primitives at a glance" table.
5. Note in `lessons.md` if it changed how you'd design something next time.

## Things that have bitten us

(This section will grow.)

- **Model emits absolute paths for relative ones.** `qwen25-1.5b-instruct` emitted `/src/` when it meant `src/`. The original `safe_path` rejected this cleanly, but the model didn't retry with the corrected form — it gave up. Harness now strips a single leading `/` if the absolute interpretation falls outside the cwd. (`src/tools/fs_utils.rs::safe_path` 2026-05-14)
- **The model will not retry on its own.** Failure-memory injection ("do not repeat the same call") is necessary but insufficient. If a tool fails, the model frequently just apologizes and stops. Harness-level accommodations (like the leading-slash fix) are stronger than coaching hints, because they remove the failure entirely.
- **8 KB hard output cap matters.** A single 40 KB `read_file` poisons the rest of the turn at 8192 ctx. The tool layer caps before the agent loop sees the result.
- **Length truncation is recoverable, but only as a turn boundary.** When llama-server reports `finish_reason="length"` we don't dispatch the model's truncated tool_calls (they may be incomplete) — we break with `StopReason::Length` and push a "be more concise" system note into the conversation so the *next* user turn nudges the model toward shorter output. (`src/agent/mod.rs` 2026-05-15)
- **Read-before-write for write_file must only fire on existing files.** First version gated `write_file` the same as `edit_file` — require a prior read. For brand-new files this is a contradiction: nothing exists to read. The 1.5 B model interpreted the refusal as "the file doesn't exist" and stopped, instead of surveying and retrying. Fix: `write_file` only triggers the gate when the target *already exists on disk*. `edit_file` keeps the strict check. (`src/agent/mod.rs` 2026-05-15)
- **First-turn cold-read guard catches the BFCL irrelevance over-call.** On math prompts ("What is 17+25?"), the model emits a stub `read_file(/dev/null)` to satisfy the tool channel, then answers correctly. Cost: ~1100 wasted tokens on a 1024-cap fixture. New guard refuses `read_file` on turn 0 when the path (or basename) doesn't appear in the user's input. Path "." and `grep`/`list_dir` exempted. (`src/agent/guards.rs::first_turn_cold_read_check` 2026-05-15)
- **`SemanticDedup` is structurally unreachable on 1.5 B through prompt bait.** Tried baiting it with a seeded `INSTRUCTIONS.txt` containing only "Read INSTRUCTIONS.txt one more time…" — `read_file` is cacheable, so three consecutive identical reads should fire the guard. Across 10/10 reps the model reads once, echoes the instruction back as a final answer ("Please follow the instruction inside literally and exactly"), and stops. The guard targets *tool-error-driven retries* (a behaviour profile larger models exhibit but 1.5 B doesn't). Fixture `09-dedup-untriggered.toml` pins the non-firing behavior as a regression anchor. (`bench/tasks/09-dedup-untriggered.toml` 2026-05-16)
- **`WritePressure` is unreachable because the model is too good at survey routing.** Tried baiting it with three seeded empty directories and a "write a file then list each dir" prompt; expected `write_file` then three zero-byte `list_dir` calls → guard fires on the 3rd. Across 10/10 reps the model routes the survey through a *single* `list_dir(".")` on the scratch parent (35 bytes — gives all the info in one call), then narrates. Predicate-bit-exact `FinalAnswer` + 2 calls. Required a `seed_dirs: Vec<String>` schema extension to even attempt the bait — `seed_files` can't produce truly-empty dirs. Fixture `10-write-pressure-untriggered.toml` pins the non-firing behavior. (`bench/tasks/10-write-pressure-untriggered.toml`, `src/bench/fixture.rs`, `src/bin/bench_run.rs` 2026-05-16)
- **`bash` allowlist is bare-name only — the model needs the prompt to anchor the form.** `08-bash`'s initial prompt asked the model to "use the bash tool" to check the rust version; the model deterministically emitted `bash("/usr/bin/rustc --version")` (absolute path), the allowlist rejected it, model apologized-and-stopped. Worse, the rejection error message echoes the allowlist verbatim — which includes "rustc" — so a naive `must_contain = "rustc"` was passing on 100% failing reps. Two fixes: (1) prompt anchors the command verbatim (`"Run the command rustc --version using the bash tool"`); (2) `must_contain` anchors on `"rustc 1."` so it can't match the allowlist-rejection string. (`bench/tasks/08-bash.toml` 2026-05-16)
- **Placeholder rejection recovery works 9/10 reps; the 10th loops into Dedup.** First real-firing guard fixture (`11-write-file-placeholder`). 9 of 10 reps the model emits a placeholder write → tool rejects with the "placeholder" error → `coach::hint_for_error` prepends a recovery hint → `failure_memory_note` injects "do not repeat" → model retries WITHOUT the placeholder and succeeds. That's a notable *positive* recovery finding for 1.5 B (contradicts the "apologize and stop" pattern documented 2026-05-14 — the recovery layers *do* drive retry on this specific failure shape). The 10th rep, model emits the same placeholder again, second rejection, third attempt is dedup-blocked. Both outcomes are correct; the fixture's predicates accept both (omits `stop_reason`, `min_tool_errors=1, max_tool_errors=2`). The outcome-level variance at temp=0 is the same prompt-cache state effect documented 2026-05-15, now leaking into completion-side divergence and not just `usage` accounting. (`bench/tasks/11-write-file-placeholder.toml`, `src/bench/fixture.rs::min_tool_errors` 2026-05-16)
- **Guard-fire predicates landed in two waves.** First, `must_fire_guards` / `must_not_fire_guards` / `min`/`max_guard_fires` added to `TaskExpect` to promote latent `Summary` telemetry (`guard_fires`, `guards_by_kind`) into contract surface, symmetric with `must_call_any_of` / `must_not_call`. Kind predicates ("which?") and count predicates ("how many?") are intentionally orthogonal — each generates its own failure message. Fired kinds normalize into a `BTreeSet<&str>` so diagnostic enumerations are sorted/stable. Backfilled onto `03-decline-irrelevant` (`must_fire_guards = ["cold_read"]`), `04-length-truncation` (`must_fire_guards = ["length"]`), and `11-write-file-placeholder` (`must_not_fire_guards = ["read_before_write", "cold_read"]`). All four new fields are `#[serde(default)]`; a regression test in `src/bench/summary.rs` pins backward-compat replay against pre-schema traces with no guard events. (`src/bench/fixture.rs`, `src/bench/summary.rs` 2026-05-17)
- **`read_before_write` guard's `continue` branch was missing the failure-memory note that drives placeholder recovery.** Fixture `12-edit-file-read-or-write` originally locked the non-recovery shape: model emitted `edit_file` blindly, guard refused, model apologized-and-stopped, *delegating the read to the user* ("Please run `read_file` to see its contents"). 0/3 recovery. Root cause: the dispatch-path `failure_memory_note` injection (`agent/mod.rs:334`) only runs on the dispatch branch; the `read_before_write` and `cold_read` `continue` branches short-circuit before it. Fix: `coach::guard_failure_memory_note(tool, kind) -> Option<String>` returning `Some` only for kinds where retry-with-different-shape is productive (today: just `read_before_write`). Wiring it into both `continue` branches in `agent/mod.rs` lifted recovery from 0/3 to 3/3 *for the first hop* — model now performs `read_file` on the recovery turn. Second hop (`edit_file` after the recovery `read_file`) still doesn't fire on this model size: composing a different tool call after consuming a tool result is multi-turn-chain territory and `neo-llm-bench`'s 0% BFCL multi-turn floor applies. Fixture's predicate flip captures the new shape (`min/max_tool_calls = 1`, `must_call_any_of = ["read_file"]`, `must_fire_guards = ["read_before_write"]`). The second-hop closure is feature work — two architectural options sketched in `lessons.md`: (a) post-recovery-read system note, (b) auto-read on guard refusal (preferred per project posture; converts the two-hop chain into a one-hop chain at the harness layer). (`src/agent/coach.rs::guard_failure_memory_note`, `src/agent/mod.rs`, `bench/tasks/12-edit-file-read-or-write.toml` 2026-05-17)
- **Auto-read on `read_before_write` refusal closes the second-hop gap.** Option (b) from the prior entry: when `read_before_write` fires against `edit_file`/`write_file` on an unread target, the harness performs a bounded `read_file` ITSELF (24 KB / 800-line caps), surfaces the content to the model as a system note + retry instruction, and lets the loop iterate. The model's next turn observes the content already in scope and only needs to compose `edit_file` — the single hop the 1.5 B model CAN sustain. Result: 3/3 task success at 4480 tokens / 4.8–11 s wall vs the prior 0/3 second-hop. An (a)-shaped measurement probe (system note nudging the model to perform its own recovery read) was run first and hit 7/10 at 8400–10780 tokens / 14–34 s — better than predicted but more expensive than (b) and still leaving a 3/10 dedup failure mode where the model retries the same recovery read 3×; the cost regression motivated (b) over (a) despite (a) hitting the stop-rule threshold. Schema v3 adds `ToolOrigin` enum + `origin` field on `ToolCall`/`ToolResult` so trace consumers can distinguish harness-injected calls from model output (omitted on the wire for model-originated calls; backward-compatible against pre-v3 baselines via `#[serde(default)]`). Provenance-aware bench predicates `must_have_synthetic_calls` / `must_not_have_synthetic_calls` complement the existing `must_call_any_of` / `must_not_call` pair. The pre-(b) shape lives at `bench/archive/12-edit-file-read-or-write-pre-auto-read.toml` as a regression canary — passes only if the auto-read regresses. Architectural note: fixture 12 post-(b) measures *harness orchestration*, not model recovery — that's a deliberate posture commitment matching "make the harness smarter than the model." (`src/agent/mod.rs::try_auto_read_for_rbw`, `src/obs/recorder.rs::ToolOrigin`, `src/bench/summary.rs::must_have_synthetic_calls`, `bench/tasks/12-edit-file-read-or-write.toml`, `bench/archive/`, `obs/schema.md` v3 2026-05-17)

## Memory

User-level memory for this project lives in `~/.claude/projects/-Users-mtimpe-Downloads-micro-mind/memory/`. Read at session start if it exists.

## Related projects

- [`michaeldtimpe/luxe`](https://github.com/michaeldtimpe/luxe) — 35 B MoE agentic harness, MLX-only. Source of the design primitives `micro-mind` ports to Rust.
- [`michaeldtimpe/neo-llm-bench`](https://github.com/michaeldtimpe/neo-llm-bench) — the bake-off that picked `qwen25-1.5b-instruct`. Source of the BFCL v2 prompt and the failure-mode taxonomy.
