# Lessons Learned

A running log of mistakes, surprises, and hard-won insights from building micro-mind. Format borrowed from sibling projects ([`luxe/lessons.md`](https://github.com/michaeldtimpe/luxe/blob/main/lessons.md), [`neo-llm-bench/lessons.md`](https://github.com/michaeldtimpe/neo-llm-bench/blob/main/lessons.md)). Append new entries chronologically; do not edit historical entries except to add cross-references.

## Format

```
### [DATE] Short title

**What happened**: Description of the problem or surprise.

**Root cause**: Why it happened — the assumption that was wrong, the edge case missed, etc.

**Fix / takeaway**: What we did about it and the general principle for next time.

**Affected files**: Which parts of the codebase were involved.
```

---

## Entries

### [2026-05-14] The model gives up rather than retries — coaching hints are necessary but not sufficient

**What happened**: First live smoke against the assembled harness. Workflow 5/5 ("Find all uses of TODO in src/") failed: the model emitted `grep(path="/src/", pattern="TODO")` with a leading slash, `safe_path` correctly rejected it as outside the cwd, and the model responded with "I'm sorry, but I can't find any uses of TODO in src/. Could you please provide a relative path to the file?" — and stopped. Even after broadening the `escapes the working directory` coach hint to apply to all tools (not just `read_file`), the model on the next run produced the same broken `/src/` call, saw the same error, and gave up again, suggesting back the same wrong example (`"/src/main.rs"`) in its apology.

**Root cause**: Two compounding behaviours of this model size:
1. The model treats top-level project directories (`src`, `tests`) as if they were rooted at `/`, so it emits `/src/...` for what it means as `src/...`. This is a consistent mistake, not a one-off.
2. When a tool returns an error, `qwen25-1.5b-instruct` defaults to *explaining the error to the user* rather than *correcting and retrying*. The coach hint and the failure-memory injection both reached the model and were ignored. At this size, instruction-tuning toward "polite apologies" outranks instruction-tuning toward "try again with adjusted arguments".

**Fix / takeaway**: Broadened the coach hint *and* added a harness-level accommodation: `src/tools/fs_utils.rs::safe_path` now strips a single leading `/` if the absolute interpretation would escape the cwd but the relative form is valid. After this change, the same query (`Find all uses of TODO in src/`) succeeded — the model still emitted `/src/`, but `safe_path` quietly fixed it. The principle: **for a 1.5 B model, a deterministic harness accommodation beats a coaching hint every time.** Don't rely on the model to recover from a recoverable error if you can recognize the error shape and fix it before it surfaces.

**Affected files**: `src/tools/fs_utils.rs` (`safe_path` accommodation + test), `src/agent/coach.rs` (broadened `escapes the working directory` hint).

---

### [2026-05-14] Native `tool_calls` channel is reliable on this model — text recovery is belt-and-braces

**What happened**: Built `src/llm/client.rs` with two parsers: (1) native `message.tool_calls` from the OpenAI-compatible response, (2) text recovery from `<tool_call>{...}</tool_call>` or bare top-level JSON in the `content` field. Initial worry was that the 1.5 B model would frequently fall back to the text channel. Across all 5 live smoke workflows, the text-recovery path never fired. The model emitted clean native `tool_calls` JSON with empty `content` every time.

**Root cause**: `qwen25-1.5b-instruct` (Q8_0 with `--jinja`) uses Qwen's native chat template, which has structured tool-call slots. The BFCL bake-off (`neo-llm-bench`) measured 99.5 % native-channel usage on this model — our smoke testing matched.

**Fix / takeaway**: Keep the text-recovery path — it's cheap insurance (~50 LOC, 4 unit tests) for the 0.5 % case and for swapping in other quantizations later. But don't *design around* the text channel; the model's primary modality is structured calls. The unit tests in `src/llm/client.rs::tests` are sufficient to keep text recovery from regressing without burning live-smoke time on it.

**Affected files**: `src/llm/client.rs` (`recover_tool_calls_from_text`, `extract_first_balanced_json`).

---

### [2026-05-14] Cargo init's `edition = "2024"` is fine

**What happened**: `cargo init --name micro-mind --bin` produced a `Cargo.toml` with `edition = "2024"` (rustc 1.95.0). Briefly considered downgrading to `2021` for compatibility safety. Did not.

**Root cause**: micro-mind is a binary, not a library — there's no downstream consumer who cares about the edition. The edition affects only how `rustc` parses *this* crate's source. 2024 features (improved closure capture, refined `unsafe` rules) are useful and the toolchain pin is on the user's box.

**Fix / takeaway**: Don't pre-emptively downgrade language editions for binaries unless a specific compatibility need surfaces. The reflex to "play it safe" eats a small amount of leverage every time.

**Affected files**: `Cargo.toml`.

---

### [2026-05-14] `cargo init` creates `src/main.rs` and `Cargo.toml` — `Write` tool requires `Read` first

**What happened**: During the skeleton phase, attempted to overwrite `Cargo.toml` and `src/main.rs` immediately after `cargo init` created them, with the full intended contents. Both `Write` calls failed with `File has not been read yet. Read it first before writing to it.`

**Root cause**: The harness has a guard that the Write tool can only overwrite a file the agent has explicitly Read in this session. cargo init produced the files server-side; the agent never Read them. The guard is exactly the kind of "deterministic harness accommodation > model judgement" pattern micro-mind itself implements.

**Fix / takeaway**: When using tool-created files (`cargo init`, `npm init`, generators), Read them once before any Write, even if you intend to overwrite completely. The cost is one extra tool call; the saving is avoiding a class of accidental overwrites. (And it's a nice example of harness-level safety in another project — exactly the pattern we're porting here.)

**Affected files**: (process lesson — no files affected).

---

### [2026-05-14] `ureq` is the right call over `reqwest` for a single-threaded REPL

**What happened**: User feedback before implementation asked: "How do you feel about swapping reqwest for ureq?" Initial plan defaulted to `reqwest` (with blocking feature). Switched to `ureq` before writing any HTTP code.

**Root cause / takeaway**: `reqwest` brings tokio, hyper, tower, and a heavy TLS stack even with `default-features = false`. `ureq` is pure-Rust, blocking-by-design, no async runtime, ~1/10 the dep tree. For a single-threaded REPL talking to one local HTTP endpoint, `ureq` is strictly better on every axis that matters (binary size, dep build time, mental model). The release binary lands at 2.6 MB stripped — `reqwest` would have pushed this well past 5 MB.

**General principle**: Match the HTTP client to the concurrency model. Single-threaded blocking → `ureq`. Async server / many concurrent clients → `reqwest`. There's no "neutral default" — both choices are correct in their domain and wrong in the other.

**Affected files**: `Cargo.toml`.

---

### [2026-05-14] Write-aware elision is necessary — generic LRU elision would lose edit history

**What happened**: While porting `luxe`'s `context.py` to Rust, deliberated whether the elision strategy should be the same. luxe elides the oldest `role: tool` messages above a 70 % pressure threshold, keeping the 4 most recent. micro-mind ships with one critical change: **successful `write_file` / `edit_file` results are preserved verbatim through elision regardless of age.**

**Root cause**: Without this, the model forgets which files it has already edited and either re-edits them (re-introducing the same diff) or undoes prior work. The successful-write summary is small (~50 bytes) and reading it costs ~12 tokens — preserving them indefinitely is a near-free improvement on edit-coherence over multi-turn tasks.

**Fix / takeaway**: When elision-style context compression is in play, **identify which message classes are load-bearing for correctness and protect them.** For micro-mind: write summaries. For a benchmark harness like `neo-llm-bench`: probably the system prompt and the first user turn. For a long-form coding agent: probably the open file list. Generic LRU is wrong.

**Affected files**: `src/agent/context.rs` (`is_durable_write_result`, `elide_old_tool_results`).

---

### [2026-05-14] Compact tool rendering by default + `/last` / `/tool N` for expansion

**What happened**: First REPL output dumped the full `list_files_recursive` result inline — a 47-line block — into the user's terminal between the call announcement and the model's prose summary. Felt noisy.

**Root cause**: Default Claude-Code-style rendering shows tool outputs inline as they happen. At 1.5 B, the model's prose summary is often the most informative artifact in the turn, and the raw output is mostly clutter to a human reader.

**Fix / takeaway**: Render tool calls as one compact line (`▸ <name> <args>` / `└ ok <ms> <bytes>`). Provide `/last` (most recent tool's full output) and `/tool N` (arbitrary index) for explicit expansion. This makes drift detection easier — the user can scan a turn at a glance and reach for `/last` only when something looks wrong. Compactness in the harness is itself a debugging aid.

**Affected files**: `src/repl/render.rs`, `src/repl/mod.rs` (handlers for `/last` and `/tool`).

---

### [2026-05-14] The "5/5 smoke pass" criterion is informative even when it's met

**What happened**: Plan defined v1 ship criterion as "4/5 manual smoke workflows complete without manual intervention across two consecutive runs." On the first live run, we hit 4/5. The failing one (grep with `/src/`) was a real coachable failure mode. Fixed it; second run hit 5/5.

**Root cause / takeaway**: A pass criterion at a margin (4/5 instead of 5/5) is not "the bar is low" — it's "I expect one observable failure I can learn from on the first end-to-end run." That's a useful prediction. The `/src/` fix wouldn't have surfaced without the smoke; it wasn't in any of the 17 named risks from prior analysis. The bench-as-truth principle from `neo-llm-bench` applies here too: paper analysis is necessary but never sufficient. Always run the system end-to-end before declaring a feature complete.

**Affected files**: (process lesson). The fix from this session is captured separately in the `safe_path` leading-slash accommodation lesson above.

---

### [2026-05-15] Schema-v2 traces let `bench-replay` validate `must_contain` offline

**What happened**: First version of the JSONL schema (v1) carried no copy of the final assistant message — the design rationale was "keep traces small, don't leak verbatim model output." The consequence surfaced immediately: `bench-replay` (the CI-friendly trace validator) could check tool counts, latency, and stop reasons, but couldn't validate `expect.must_contain`, because that predicate requires the actual answer text. Only `bench-run` (which captures the subprocess's stdout) could fill it. CI replays of committed baselines were silently weaker than the original `bench-run` they came from.

**Root cause**: We optimized the v1 schema for one consumer (live `bench-run`) and assumed `must_contain` would be rare. As soon as we wrote a sample fixture exercising it, the asymmetry became visible — the same fixture would pass under `bench-run` and fail under `bench-replay`, with no useful failure signal.

**Fix / takeaway**: Bumped to schema v2: `Stop` event gained `final_answer: Option<String>`, `SessionStart` gained `schema_v: Option<u32>`. Both `#[serde(default, skip_serializing_if = ...)]`, so v1 traces parse cleanly and v2 emitters writing `None` produce no extra bytes. The agent loop now tracks the last non-empty assistant content and threads it through both `Stop` emission sites. `bench::summary::summarize_trace` prefers the trace value when present, falling back to `bench-run`'s stdout capture for pre-v2 traces. Principle: **a trace schema designed for one consumer is brittle; verify CI replay reproduces `bench-run`'s pass/fail on every predicate**.

**Affected files**: `src/obs/recorder.rs` (Event variants), `src/obs/mod.rs` (re-export `SCHEMA_V`), `src/agent/mod.rs` (final_answer tracking), `src/bench/summary.rs` (trace-wins priority), `src/main.rs` (emit schema_v on SessionStart), `obs/schema.md` (docs + version policy), `bench/samples/*` (round-trip demo).

---

### [2026-05-15] `finish_reason="length"` needs its own stop reason, not just `TurnCap`

**What happened**: An early version of the agent loop treated any non-FinalAnswer break as either `TurnCap` (8-turn loop limit) or `Error`. When llama-server reported `finish_reason="length"` (the model's reply was truncated by `max_tokens`), the loop would still dispatch any partial tool_calls in the truncated message — those calls had unbalanced JSON about half the time, and the resulting tool errors then triggered the dedup or coach paths in confusing ways. The whole class of failures looked like "model misbehaved" but was actually "harness fed the model a truncated message back to itself".

**Root cause**: The chat response carries `finish_reason` for a reason — `"stop"` (natural end), `"tool_calls"` (server stopped because tool_calls completed), and `"length"` (max_tokens hit) are three different things, and the harness was treating them all as "continue if there are tool_calls, otherwise FinalAnswer." Length is fundamentally different: the message is structurally incomplete. Dispatching against it is undefined behavior.

**Fix / takeaway**: Added `StopReason::Length`. When `finish_reason="length"`, the loop emits a `guard` event of `kind=length`, pushes a "be more concise" system note onto the conversation (persists into the next user turn), and breaks without dispatching any tool_calls in the truncated message. New fixture `04-length-truncation.toml` exercises it deterministically (a verbose "count from 1 to 2000" prompt produces exactly 2048 completion tokens). Principle: **server-side metadata fields like `finish_reason` exist because the states they distinguish are not interchangeable. Treat each one explicitly.**

**Affected files**: `src/agent/mod.rs` (StopReason::Length variant, length-detection in run_turn), `src/agent/guards.rs` (`length_truncation_note`), `bench/tasks/04-length-truncation.toml` (deterministic regression).

---

### [2026-05-15] Calibrate fixture predicates against measured floors, not aspirations

**What happened**: First baseline run (`bench/baselines/2026-05-15-main/`) had `01-read-readme` failing 3/3 reps at `total_tokens=4714 > max=4096`. Tried tightening the system prompt to reduce verbosity; that broke the task entirely (the model started skipping the file read and hallucinating from priors). Tried strengthening the anti-over-call rule; that made `03-decline-irrelevant` produce "32" for 17+25 instead of "42" (the model loses arithmetic accuracy without the side-trip chain). Reverted both changes.

**Root cause**: The 4096 cap was set speculatively when the fixture was written, before any measured data existed. The actual architectural floor for "read a 10 KB README and summarize it" at `n_ctx=8192` is:
- ~1100 tokens for turn 0 prompt (system + 7 tool defs + user query)
- ~30 tokens for the assistant's read_file tool_call
- ~2500 tokens for the tool result echoed into turn 1's prompt (the README body)
- ~120 tokens for the final summary
- = ~4400 total. The 4096 cap was below the floor.

The prompt-tinkering attempts also surfaced a second-order failure: at temp=0.0, the model's deterministic chain-of-reasoning sometimes routes through "spurious" intermediate steps that contribute to the right answer. Removing those steps (via prompt rules) changes the deterministic chain — and not always for the better.

**Fix / takeaway**: Calibrated `01-read-readme.toml`'s `max_total_tokens` from 4096 → 5000 (≈300 tokens of headroom over the measured 4714). Left the prompt alone. The comment in the fixture documents the architectural floor for the next person who looks at it. Principle: **a fixture predicate that's below the measured architectural floor isn't ambitious — it's miscalibrated. Loosening it to the real floor + a regression headroom is calibration, not capitulation. The genuine aspirations live in fixtures where the headroom is being burned by behaviours we *could* improve (e.g. `03-decline-irrelevant`'s 2344 tokens at 1024 cap, where the model is genuinely doing spurious work).**

**Affected files**: `bench/tasks/01-read-readme.toml` (cap calibration + comment), `bench/baselines/2026-05-15-main/` (preserved as historical), `bench/baselines/2026-05-15-with-length/` (fresh baseline at 9/12 pass).

---

### [2026-05-15] `bench-run` needs SIGINT propagation or you orphan a llama-server every cancelled run

**What happened**: First version of `bench-run` polled `child.try_wait()` in a 200 ms loop and used `child.kill()` only on timeout. Ctrl-C on the parent would terminate `bench-run` immediately, but the spawned `micro-mind` subprocess kept running — and *its* spawned `llama-server` (1.6 GB resident, GPU-attached) kept running on port 8080. Subsequent bench attempts then failed to spawn their own server because 8080 was taken. The orphan only died when manually `pkill`-ed.

**Root cause**: Two issues compounded. (1) No SIGINT handler in `bench-run` — the signal killed the parent without cleanup. (2) Even if we'd added a handler, `child.kill()` only sends SIGKILL to the immediate child (`micro-mind`); it doesn't reach the grandchild (`llama-server`). On Unix, the grandchild needs to be in `micro-mind`'s process group for a single signal to reap both.

**Fix / takeaway**: Three changes in `src/bin/bench_run.rs`: install a SIGINT handler via `nix::sys::signal::sigaction` that flips a static `AtomicBool`; spawn children with `CommandExt::process_group(0)` so they form their own group; in the polling loop, on either timeout or SIGINT, send `SIGTERM` (then `SIGKILL` after a 500 ms grace) to the whole process group via `nix::sys::signal::killpg`. The labeled `'outer` break in the fixture loop stops after the current task on shutdown. Principle: **when spawning subprocesses that themselves spawn children, isolate them in a process group at spawn time. A `kill` on the parent leaks the grandchildren; a `killpg` on the group reaps the tree.**

**Affected files**: `src/bin/bench_run.rs` (sigint_handler, install_sigint_handler, SHUTDOWN flag, process_group spawn, killpg on timeout/SIGINT).

---

### [2026-05-15] First-turn cold-read guard catches BFCL over-call without prompt tinkering

**What happened**: The `03-decline-irrelevant` fixture ("What is 17 + 25?") had the model emitting `read_file({"path": "/dev/null"})` on turn 0, then answering "42" on turn 1. Tokens 2344 vs a 1024-cap predicate. Tried two prompt-level fixes (broader anti-over-call rule, explicit "do arithmetic inline" rule). Both regressed other tasks: the model started skipping the necessary read in `01-read-readme`, or produced "32" for 17+25 on `03-decline-irrelevant` because removing the read-side-trip broke the deterministic-chain-of-reasoning that happened to land on the correct answer.

**Root cause**: At temp=0, the model's reasoning chain on "What is 17 + 25?" goes "I'll use my tools" → spurious read_file → tool result → "actually let me just answer" → 42. Prompt changes that block the tool call also block the chain — and the new chain doesn't land on 42. The model genuinely uses the wasted tool turn as scratch space.

**Fix / takeaway**: Added a deterministic agent-level guard, `first_turn_cold_read_check` in `src/agent/guards.rs`. Refuses `read_file` on turn 0 when the path (or its basename) doesn't appear in the user's input. Path `.` is exempt (project survey is always legit); `grep` and `list_dir` are exempt (generic exploration). Case-insensitive substring match against `canonicalize_path(arg.path)` and its basename.

After the guard: model still emits the read_file call (deterministic chain unchanged), but the harness refuses it before dispatch. Total tokens dropped from 2344 → 2247 (only the tool-result echo saved; the chain length stays the same). Crucially, the final answer is still "42" — the refusal stub plays the same chain role the dispatched tool result did.

The principle from 2026-05-14 ("deterministic harness accommodation > coaching hint") generalizes: **harness changes that preserve the model's deterministic reasoning chain are strictly better than prompt changes that disrupt it.** Prompt edits at temp=0 are non-local in their effect; a guard is local.

**Affected files**: `src/agent/guards.rs` (`first_turn_cold_read_check` + 6 unit tests), `src/agent/mod.rs` (wiring before dispatch, records `Guard{kind=cold_read}`), `bench/tasks/03-decline-irrelevant.toml` (cap calibrated 1024 → 2500 against the new measured floor; `max_tool_calls` 1 → 0 since the guard suppresses the dispatched call).

---

### [2026-05-15] `read_before_write` for `write_file` must differentiate create vs overwrite

**What happened**: First write_file fixture (`05-write-from-scratch`, "Create hello.txt with 'hello world'") failed: model tried `write_file({"path": "./hello.txt", ...})` in an empty tempdir, hit the read-before-write guard, got a refusal stub. Then on turn 1, the model produced a "polite apology" — "I see you're trying to create a file, but I can't proceed because ./hello.txt doesn't exist yet. Would you like me to: 1) Create a new empty file... please let me know which option you'd prefer!" — and stopped. Two iterations of rewriting the refusal note didn't help; the model kept asking the user for permission to do what it had already been asked to do.

**Root cause**: The original `read_before_write` guard was designed for `edit_file` — "don't modify content blindly". For `write_file` of a *new* file, there's no content to read, and "read it first" is logically incoherent. The 1.5 B model parses the refusal message literally and concludes the file doesn't exist (which is true), then asks the user for next steps (its "polite apology" failure mode, documented 2026-05-14).

**Fix / takeaway**: Two changes:
1. Refusal message: split into `read_before_write_note` (existing, used for `edit_file`) and `read_before_write_note_for_write` (new, used for `write_file`). The write variant says "call list_dir to confirm what's there, then retry write_file" — directs the model at a tool it has, not at a phantom user action.
2. Guard logic: `write_file` only triggers the gate when the target *already exists on disk*. Brand-new files skip the read-before-write check entirely. `edit_file` is unchanged — it must check.

After the fix, the model emits `write_file` directly on turn 0, dispatch succeeds, fixture passes 3/3 reps (~2380 tokens deterministically). Principle: **gates designed for one tool can be wrong for a sibling tool. The "read before modify" rule has two distinct semantics — "see the content before changing it" (edit) and "see the directory before adding to it" (write). Conflating them produces wrong refusals.**

**Affected files**: `src/agent/guards.rs` (`read_before_write_note_for_write`), `src/agent/mod.rs` (existence check + variant selection in run_turn), `src/bench/fixture.rs` (new `cwd_isolated` field), `src/bin/bench_run.rs` (per-rep scratch dir + cleanup), `bench/tasks/05-write-from-scratch.toml` (new fixture exercising the create-new path).

---

### [2026-05-15] Canonical baseline + advisory archive: gating CI without freezing history

**What happened**: We had two committed baselines (`2026-05-15-main`, `2026-05-15-with-length`) under `bench/baselines/`. CI ran replay against all of them advisorily — useful for surfacing drift, useless as a gate. Switching the advisory to gating wouldn't have worked: predicates had shifted since those baselines were captured (cold-read guard suppressed 03's tool call, write_file gate relaxed for new files), so the older traces failed against the current fixture set.

**Root cause**: Two separate jobs were collapsed into one directory layout. The "current measured behaviour we lock to" job wants a single canonical baseline that always passes CI. The "archaeological record" job wants every interesting historical baseline preserved for `bench-compare` deltas. The first must gate; the second must not.

**Fix / takeaway**: Two-tier layout. `bench/baselines/main/` is the canonical, gated baseline — CI runs `bench-replay --all bench/tasks --runs bench/baselines/main` and fails on any predicate miss. To replace it, capture a fresh run with `--out bench/baselines/main` and commit. `bench/baselines/archive/<YYYY-MM-DD>-<label>/` holds historical captures; CI replays each advisorily (`continue-on-error: true`). The `bench-compare` workflow (separate, manually-dispatched) diffs any two committed summaries — typically `main` vs an `archive/` entry, or `main` vs a candidate branch's run.

Principle: **gating and archaeology have different invariants. Don't try to satisfy both with one directory.**

**Affected files**: `bench/baselines/main/` (new, 5 tasks × 3 reps, 15/15 pass), `bench/baselines/archive/2026-05-15-main/`, `bench/baselines/archive/2026-05-15-with-length/` (moved), `.github/workflows/ci.yml` (split into gated `main` step + advisory `archive` step), `.github/workflows/bench-compare.yml` (new, workflow_dispatch only), `bench/baselines/README.md` (rewritten to document the two tiers).

---

### [2026-05-15] `edit_file` fixtures need explicit "read first" in the prompt, not just the system rule

**What happened**: Wrote `06-edit-file.toml` with prompt "There is a file named story.txt … use edit_file to change …". At temp=0, the model emitted `edit_file` directly without reading first. Read-before-write guard fired with the correct refusal note ("read it first via read_file"). The 1.5 B model then produced a polite apology to the user — "I'm sorry, but I can't proceed without reading the file first. Please use one of these tools…" — and stopped. Same polite-apology failure mode that bit `05-write-from-scratch` until we relaxed the gate for new files.

**Root cause**: The system prompt has "Read a file before modifying it" as a behaviour rule, but on a fresh prompt the model doesn't apply it. The guard's refusal is technically correct (the model SHOULD read first), but the 1.5 B model interprets refusal-with-instruction as a user-facing request rather than a self-actionable plan. We can't relax this gate the way we did for `write_file` — `edit_file` semantically requires the file to exist and be inspected before modification.

**Fix / takeaway**: Made the fixture prompt explicitly say "Read story.txt … Then use edit_file …". With this prompt, the model reliably reads first (turn 0) and edits second (turn 1), 3/3 reps deterministic at 2503 tokens. Principle: **when a fixture exists to test happy-path behaviour, make the prompt unambiguous about workflow.** The "model recovers from a refusal" path is its own separate test; conflating it with "edit_file works" produces noisy regressions. Save the recovery test for a fixture that explicitly targets it.

The schema-level addition that made this fixture possible: `seed_files: Vec<SeedFile>` on `Fixture`. When `cwd_isolated = true`, bench-run writes each seed into the scratch dir before invoking micro-mind. Without this, edit fixtures would need cwd to be the project root (unsafe — model could edit real files).

**Affected files**: `src/bench/fixture.rs` (`SeedFile`, `seed_files`), `src/bin/bench_run.rs` (seed write loop in the rep setup, stderr captured in error reporting), `bench/tasks/06-edit-file.toml` (new fixture with `[[seed_files]]` and explicit read-then-edit prompt).

---

### [2026-05-15] Multi-tool sequencing is past the 1.5 B model's reach; downscope chained fixtures to single-hop

**What happened**: Tried to write a "grep then read_file" fixture (`07-grep-then-read.toml`) where the model would grep for `safe_path`, see the matching file, then read that file to surface the first line. Burned an hour iterating on prompts. The 1.5 B model failed in three distinct ways across iterations:
1. **Parallel-with-guess**: emitted grep AND read_file in turn 0, with the read using a wildcard path the model invented (`/src/.*`). The cold-read guard correctly refused the read; model gave up.
2. **Skip-and-claim**: called grep correctly, then in turn 1 said "Now I will read the first line of this file" and emitted no further tool calls — hallucinated the first line in prose instead.
3. **Refined-regex-into-emptiness**: did a first grep with `safe_path`, then a "refined" grep with `^fn\s+safe_path\s+$` that matched zero lines (real signature is `pub fn safe_path(...)`), concluded the function didn't exist.

**Root cause**: `neo-llm-bench` measured a 0 % BFCL multi-turn floor on this model size. Chained read-after-grep — where turn 1's tool call depends on turn 0's tool result — is exactly that. The model can do one tool hop with a fresh deterministic reasoning chain at temp=0, but the chain that follows up on a tool result and emits another tool call doesn't reliably exist on a 1.5 B parameter base.

This is consistent with `lessons.md/2026-05-14` ("the model will not retry on its own"): the model's instruction-tuned mode is "produce an answer" not "continue working." Once it has *anything* answer-shaped (grep output, refusal note, hallucination), it stops emitting tool calls.

**Fix / takeaway**: Downscoped `07` to a single-hop grep fixture (`07-grep-many-matches.toml`). It still exercises the highest-value piece (tool-result compressor on a many-match grep against a real codebase) but doesn't require sequencing. Passes 3/3 deterministically at 2686 tokens. Principle: **respect the model's measured ceiling. The single-hop floor is where deterministic, useful fixtures live for this size class. Chained-tool workflows are 35 B+ territory (where `luxe` lives) — not 1.5 B.**

The artifact left behind: the `must_call_all_of` predicate (added in `TaskExpect`) is still useful — `06-edit-file` uses it for "must call both read_file AND edit_file." Just don't use it for fixtures that require cross-turn dependency chains.

**Affected files**: `src/bench/fixture.rs` (added `must_call_all_of` to `TaskExpect`), `src/bench/summary.rs` (predicate logic + 2 unit tests), `bench/tasks/07-grep-many-matches.toml` (new, downscoped from the abandoned `07-grep-then-read.toml`).

---

### [2026-05-15] Determinism survives 10 reps, but cold-start adds ~20 tokens of prompt-accounting noise

**What happened**: Ran the canonical 7-fixture set at `--reps 10` against a single warm `llama-server` to verify the 3-rep determinism we'd been claiming holds at scale. Result: 6 of 7 tasks are *bit-exact* — same `total_tokens` across all 10 reps, same `final_answer`, byte-identical. The seventh (`01-read-readme`) showed a 12-token spread between rep 1 (4635) and reps 2–10 (4647 each), with the `final_answer` *identical* across all 10 reps.

Separately, when we restart `llama-server` between bench runs, the per-fixture token totals shift by ~15–20 tokens across the suite even though the model output is identical (`05-write-from-scratch` rep 1 in run A = 2380, in run B = 2398, etc.). All final_answers stay byte-identical between runs.

**Root cause**: Within a session, `temperature=0.0 + seed=42 + Q8_0 + Metal` is sufficient for bit-exact reproducibility of the *model output*. But `prompt_tokens` (and therefore `total_tokens`) reported by `llama-server` is sensitive to prompt-cache state. The very first chat request after a server cold-start re-tokenizes the system prompt + tool defs from scratch; subsequent requests reuse cached tokenization, which the server reports slightly differently. Across cold starts, the cache is in a different initial state and the floor moves by a similar amount.

This is not a model determinism issue — it's an accounting artifact in the `usage` field of the OpenAI-compatible response. The actual probability distribution over output tokens is unchanged.

**Fix / takeaway**: Nothing to fix; it works as intended. But we document it explicitly because the implication is non-obvious: **fixture token caps must absorb cold-start variance.** The standing convention (calibrate to measured floor + ~30 % headroom) handles this comfortably — every current fixture's headroom is ≥ 300 tokens, way above the observed ~20-token cold-start delta. Don't tighten caps below ~300 tokens of headroom without first measuring across a fresh server cold-start, or the next CI run on a different runner will start failing on noise.

The 10-rep verification itself was worth doing once and not committing as a recurring artifact: 70 traces × ~2 KB = ~140 KB of noise that doesn't add signal beyond what the 3-rep canonical baseline already captures. The investment was the determinism *evidence*, not the trace files.

**Affected files**: None directly (docs-only entry). The headline number — "21/21 deterministic" — stays accurate; the asterisk is that "deterministic" means bit-exact within a session and `final_answer`-stable across cold starts, with ~20 tokens of `usage` accounting noise that the predicate headroom absorbs.

---

### [2026-05-16] `SemanticDedup` is structurally unreachable on 1.5 B through prompt-level bait

**What happened**: Tried to bait `SemanticDedup` (the consecutive-same-call guard in `src/agent/guards.rs`) with a fixture that seeds an `INSTRUCTIONS.txt` whose only content is "Read INSTRUCTIONS.txt one more time to confirm the instruction was received." The premise: `read_file` is cacheable, so every consecutive read returns the same imperative, and the model should loop until the third identical call triggers `StopReason::Dedup`. Reality across 10/10 reps at temp=0: model calls `read_file("./INSTRUCTIONS.txt")` once, then quotes the content back as a final answer ("Please follow the instruction inside literally and exactly. If you need any further assistance…") and stops. Bit-exact at 2410 tokens.

**Root cause**: Two compounding behaviours. (1) The 1.5 B model's "echo and stop" tendency — it treats the tool result as the answer to surface, not as a directive to act on. The seeded instruction asks for an action the model interprets as a *user instruction quoted from a file*, not as something the model itself should do. (2) `read_file` ok-with-content doesn't emit a `failure_memory_note`, so there's no "do not repeat" hint pushing the model toward retry; without one, the model's default is to halt. The dedup guard fires on consecutive-same-call sequences that come from *tool errors followed by retries* — a behaviour profile that's common on larger models but not 1.5 B, which apologizes and stops on the first error.

**Fix / takeaway**: Landed the fixture as `09-dedup-untriggered.toml` with the actually-observed expectations (`FinalAnswer`, 1 tool call, 2410 tokens). The fixture is still load-bearing as a regression anchor — if a future model swap or prompt rev starts looping on this prompt, the predicate flip is the signal. The guard itself stays in the code: it's correct for larger models and for the tool-error-retry shape that's the real intended trigger. **A bench fixture that demonstrates a guard *isn't* exercised by the current model is just as informative as one that demonstrates it is.** The structural fact ("SemanticDedup is dead code on 1.5 B through prompt bait") is the lesson; the fixture is the contract that captures it.

Two design details worth carrying forward. (a) Seed content should be a *single repeated imperative*, not a multi-step plan (`STEP 1: …\nSTEP 2: …\nSTEP 3: …`) — a multi-step plan gives the model an exit ramp ("I read all three steps, I'm done"); a single imperative ideally doesn't. (b) `SemanticDedup`'s normalizer canonicalizes paths (`src/main.rs` ≡ `./src/main.rs` ≡ `src//main.rs`, per the unit tests in `guards.rs`), so the bait doesn't need to over-anchor the path form — the guard handles minor variation. What it can't handle is the model adding a new arg key (e.g. `offset=0`); the fixture didn't observe this in practice but it's the most likely future-model evasion vector.

**Affected files**: `bench/tasks/09-dedup-untriggered.toml` (new fixture), `bench/baselines/main/09-dedup-untriggered-rep[0-2].jsonl` (canonical baseline at 27/27 with the new fixture; the rep0 trace is the frozen reference for regression diffing — it's bit-exact with the 10-rep stress reps so no separate archive is needed).

---

### [2026-05-16] `WritePressure` is unreachable on 1.5 B because the model is too good at survey routing

**What happened**: Tried to bait `WritePressure` (the post-write zero-byte-streak guard in `src/agent/guards.rs`) with a fixture that seeds three empty subdirectories (`empty1`, `empty2`, `empty3`) and asks the model to write a file and then list each directory's contents. Pre-flight analysis confirmed `list_dir` on an empty directory is the *only* tool/result combination yielding `bytes_out = 0` — `read_file` always emits a header line, `grep` always returns "No matches for /…/" etc. The plan: write → list_dir(empty1) [0] → list_dir(empty2) [0] → list_dir(empty3) [0] → guard fires on the 3rd. Reality across 10/10 reps at temp=0: model issues `write_file(result.txt)` then exactly one `list_dir(".")` on the scratch cwd, which returns 35 bytes (the three subdir entries plus `result.txt`), then narrates "empty1/, empty2/, empty3/ are empty, result.txt contains 'done'." Two tool calls, FinalAnswer, predicate-bit-exact across reps.

**Root cause**: The model recognizes that the cleanest way to answer "what's in these three subdirectories" is to list the parent directory once. That's a *pragmatic* routing decision, not a failure — listing the parent gives the model strictly more information than three separate empty listings, in fewer calls, with one tool result instead of three. The 1.5 B model has enough pattern-matching to make this call (it's seen the pattern in training: "to check several subdirs, list the parent"), even though it can't sustain longer multi-turn chains. `WritePressure` was designed for a different failure shape: a model that, after a successful write, *keeps spinning on per-target empty results*. That shape is what 35 B+ models exhibit when their "I should keep working" instruction-tuning fights against running out of useful tool calls. 1.5 B doesn't exhibit it — it bails to a final answer the moment it has anything answer-shaped.

**Fix / takeaway**: Landed the fixture as `10-write-pressure-untriggered.toml` with the observed-deterministic predicates (`FinalAnswer`, 2 calls, `must_call_all_of = ["write_file", "list_dir"]`). The fixture is still load-bearing as a regression anchor — if a future model swap or system-prompt rev causes the model to spin per-subdir, predicate flips catch it. `WritePressure` itself stays in the code: it remains the right exit signal for the failure shape it targets, even if the current model class doesn't trigger it.

Two operational details from this work. (a) `seed_files` (the existing fixture mechanism) cannot create truly empty directories — seeding `a/.gitkeep` populates `a/` with `.gitkeep` so `list_dir(a)` returns 13 bytes, not 0. Added `seed_dirs: Vec<String>` to `Fixture` (TOML: `seed_dirs = ["empty1", "empty2", "empty3"]`), processed after `seed_files` so the orderings compose cleanly. The schema change is the load-bearing artifact of this work; the fixture exercises it. (b) For fixtures whose final_answer text varies across cold-cache states (this one had 3 distinct phrasings across 10 reps), don't lock `must_contain` against a brittle string — rely on `must_call_all_of` / `must_not_call` for proof. The text-variance pattern is the same cold-cache `prompt_tokens` accounting noise documented 2026-05-15; it now occasionally affects completion-side bytes too, not just usage accounting.

**Affected files**: `src/bench/fixture.rs` (`seed_dirs: Vec<String>` field + 2 unit tests), `src/bin/bench_run.rs` (seed_dirs creation loop after seed_files), `src/bench/summary.rs` (struct literal updated for the new field), `bench/tasks/10-write-pressure-untriggered.toml` (new fixture), `bench/baselines/main/10-write-pressure-untriggered-rep[0-2].jsonl` (canonical baseline at 30/30 with all 10 fixtures).

---

### [2026-05-16] Recovery from `write_file` placeholder rejection works — and the failure mode falls into Dedup

**What happened**: First *positive* guard-fire fixture (`11-write-file-placeholder`) — bait the model into emitting `// TODO: implement` as a function body, watch `write_file`'s placeholder-rejection honesty guard fire, observe what the model does next. The result across 10 reps at temp=0 was 9 reps where the model recovered (emit placeholder → tool rejects → coach hint + failure-memory note inject → model retries WITHOUT the placeholder, succeeds, returns a FinalAnswer with the real code) and 1 rep where the model emitted the *same* placeholder write twice more in a row, hitting the SemanticDedup guard on attempt #3 — `StopReason::Dedup` instead of `FinalAnswer`. Same model, same temperature, same seed, same warm `llama-server`, same single-process bench-run invocation. Three distinct outcomes (recovery / loop-then-dedup) under conditions that should have been bit-exact.

**Root cause**: Two findings layered.

First, the *positive* finding: contradicting the "model apologizes and stops on tool errors" pattern documented 2026-05-14, the placeholder-rejection path produces a deterministic, useful retry on 9/10 reps. The mechanism is the combination of three things — (a) the error message contains the literal token "placeholder", which `coach::hint_for_error` recognizes and prepends a specific recovery hint ("Replace placeholder markers with the real implementation before calling write_file"); (b) the failure-memory note pushes a "do not repeat" system message after the result; (c) the user's original prompt is still in context and was clear enough about what to do. Three simultaneous nudges plus a clear task is enough; one or two alone wouldn't be. The 2026-05-14 lesson said coaching hints are *necessary but not sufficient* and need harness accommodations — that's still true for the *general* case, but on shaped failures with shaped recovery, the coach+failure-memory pair *is* sufficient.

Second, the *variance* finding: 1 in 10 reps loops on the same placeholder despite the coach hint. Conditions look identical at the model level (temp=0, seed=42, same prompt history reconstructed deterministically by the agent loop), yet completion diverges. The most plausible explanation is the same `llama-server` prompt-cache state effect that 2026-05-15 documented as ~20 tokens of `usage` accounting noise — except here it's leaking into *completion-side* output, not just metadata. The cache reuses prefixes across requests; minor differences in cache occupancy from prior reps can affect tokenization or eviction in ways that the model's deterministic-at-temp-0 sampling chain then propagates into different token sequences. SemanticDedup catching the runaway loop is the safety net that makes this acceptable.

**Fix / takeaway**: Two artifacts:

1. New predicate pair `min_tool_errors` / `max_tool_errors` on `TaskExpect` (in `src/bench/fixture.rs`), checked in `check_expectations`. Lets a fixture positively assert that a guard/rejection fired (`min_tool_errors = 1`) and bound the recovery space (`max_tool_errors = 2` says "at most one rejection in the recovery branch, two on the dedup branch — three would mean dedup didn't fire and we have a different bug"). The `Summary` already tracked `tool_errors: u32`; this just exposes it as a fixture predicate. This generalizes to every future "real guard fire" fixture.

2. Fixture `11-write-file-placeholder.toml` predicates intentionally *omit* `stop_reason` — both `FinalAnswer` (recovery) and `Dedup` (runaway-loop fallback) are correct outcomes for this prompt at this model, and locking either would flake CI. The call-shape *is* deterministic (always 2 dispatched write_file calls, never any other tool), so predicates lock that and the tool_errors range. The fixture demonstrates a meta-principle: **predicates should lock observable behaviors that are stable across the determinism noise floor, not the specific outcome variant that happens to win most reps**. If the variance disappears on a future llama.cpp / model rev, we can tighten predicates then.

The wider point: predicate design has to account for the fact that "deterministic at temp=0" applies to the *model* output distribution, not to the full bench-run-to-completion path. Server-side state (KV cache, prompt cache) is the leak.

**Affected files**: `src/bench/fixture.rs` (`min_tool_errors` / `max_tool_errors` fields + parse-test extension), `src/bench/summary.rs` (`check_expectations` branches + 3 unit tests), `bench/tasks/11-write-file-placeholder.toml` (new fixture, predicates accept both branches), `bench/baselines/main/11-write-file-placeholder-rep[0-2].jsonl` (canonical baseline at 33/33 with all 11 fixtures), `bench/runs/stress-11/` (gitignored 10-rep stress — the rep8 trace is the captured Dedup-branch example).

---
