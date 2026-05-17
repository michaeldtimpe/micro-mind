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

### [2026-05-17] `read_before_write` guard does not recover the way `write_file`'s placeholder guard does — structural reason, not model intelligence

**What happened**: Second positive guard-fire fixture (`12-edit-file-read-or-write`) bait `read_before_write` on the `edit_file` path with a naturally underspecified prompt — "Change 'foo' to 'bar' in notes.txt." Pre-flight expectation: either the model reads first (Outcome A — third non-fire anchor alongside 09/10) or edits blindly and the harness layers drive recovery (Outcome B — analog of `11-write-file-placeholder` which recovers 9/10 reps via coach hint + failure-memory note). What actually happened across 3 reps at temp=0, bit-exact at 2411 total_tokens: the model emits `edit_file` on turn 0 without reading, the `read_before_write` guard fires and pushes a refusal note as a `tool_result`, and the model produces a final answer instructing **the user** to "Please run `read_file` to see its contents" (one rep even dictated a bash command with the wrong shape: `bash "cat …" --timeout_s 30`). Zero retry, zero recovery, `stop_reason=FinalAnswer`.

**Root cause**: Asymmetric agent-loop wiring, not a model capability gap. The placeholder-rejection path goes through `dispatch()` in `src/agent/mod.rs:296`, which sets up `coach::coach` (line 306) and `coach::failure_memory_note` (line 334) to fire on the tool result — meaning the 1.5 B model receives THREE nudges toward retry: the error message itself with the "placeholder" keyword, a recovery hint prepended by the coach, and a "do not repeat" system note. The `read_before_write` check at `src/agent/mod.rs:256` fires *before* dispatch, ends the iteration with `continue`, and pushes only the refusal note as a `tool_result` — coach and failure-memory both skipped. The 1.5 B model on a single nudge with no explicit "retry this with a different shape" cue falls back to its instruction-tuned default of "produce an answer," and the answer it produces is "tell the user how to do it." (See `lessons.md` 2026-05-14: "the model will not retry on its own.")

The model's instruction-tuned habit of *delegating back to the user* is itself worth flagging — it produced a markdown code block with a tool-call-shaped literal (`bash "cat …" --timeout_s 30`) but never emitted the structured `tool_call`. The chat template's tool-call channel was open; the model chose prose roleplay instead. Same trace shape as the "echo the file content as the final answer" behavior on 09's `INSTRUCTIONS.txt` (2026-05-16) — both are instances of the model treating tool work as something to *describe* rather than *do*.

**Fix / takeaway**: Two artifacts.

1. Landed the fixture as `12-edit-file-read-or-write.toml` with the observed-deterministic predicates: `stop_reason = "FinalAnswer"`, `min/max_tool_calls = 0`, `must_fire_guards = ["read_before_write"]`, `min/max_guard_fires = 1`, and a defensive `must_not_fire_guards` set covering the other five guard kinds. The fixture pins the *non-recovery* shape as a regression anchor — if a future harness change (e.g. wiring `coach::hint_for_error` into the guard path before the `continue`) drives recovery, the `max_tool_calls = 0` predicate flips RED, which is the exact signal we want.

2. **No fix in agent code this cycle.** Wiring the coach into the guard path is the obvious next move — and would likely lift this from a 0/3 to a 9/10 recovery rate matching the placeholder case. But that's a deliberate architectural change with its own design questions (does every guard get a recovery hint? what's the hint for `dedup` or `turn_cap`? do we add `coach::hint_for_guard(kind)`?). It's not a bug fix; it's a feature. Capturing the *current* behavior as a fixture first means we have a baseline to ratchet against when that feature lands.

The meta-principle: **guard semantics live in two places — the predicate code AND the recovery affordance**. The predicate code is straightforward (refuse the dispatch, push a note). The recovery affordance — what the model is *told to do next* — is what determines whether the guard becomes a productive intervention or a dead end. The placeholder guard is productive because it inherits dispatch-path coaching. The read-before-write guard is not productive because it short-circuits dispatch. Symmetric harness behavior on different guard kinds is itself a design choice.

**Predicate-design note**: This is also the first use of the new `must_fire_guards` / `min_guard_fires` / `max_guard_fires` predicates landed in the same release wave. The fixture exercises all three orthogonally: kind ("read_before_write fired"), count floor ("at least one"), count ceiling ("exactly one"). The orthogonality matters because a future harness change that triggers an extra unrelated guard would surface as a count violation, not a kind violation — different bug, different fix. Diagnostics enumerate fired kinds from a `BTreeSet`, so failure messages stay stable.

**Affected files**: `bench/tasks/12-edit-file-read-or-write.toml` (new fixture), `bench/baselines/main/12-edit-file-read-or-write-rep[0-2].jsonl` (canonical baseline at 36/36 with all 12 fixtures), `bench/baselines/main/summary.json` (3 outcomes appended, `n_outcomes` 33→36).

> **Update 2026-05-17 (same day):** This entry's 0/3 recovery claim is **superseded** by the next entry. Wiring `coach::guard_failure_memory_note` into the `read_before_write` guard branch lifted recovery from 0/3 to 1/2-of-the-way: the model now performs the first recovery hop (`read_file`) but stops before the second hop (`edit_file`). The structural diagnosis above stands — the guard path was missing the "do not repeat" nudge — but the *consequence* changed once the nudge landed.

---

### [2026-05-17] Failure-memory wiring closes the first recovery hop; the second hop is multi-turn-chain territory the 1.5 B model can't sustain

**What happened**: Acted on the prior entry's "obvious next move" by wiring `coach::guard_failure_memory_note(tool, kind) -> Option<String>` into the `read_before_write` guard branch in `src/agent/mod.rs`. Function returns `Some` only for guard kinds where retry-with-different-shape is the productive outcome (today: just `read_before_write`); `cold_read`, `dedup`, `write_pressure`, `length`, `turn_cap` all return `None` so the new wiring is a no-op for them. Re-ran `12-edit-file-read-or-write` to verify. Result across 3 reps at temp=0, bit-exact at 3881 total_tokens (was 2411 pre-wiring, +1470 from the recovery context):

- **turn 0**: model emits `edit_file` → `read_before_write` guard refuses → refusal note pushed (already had the recovery instruction) + failure-memory note pushed (the new piece)
- **turn 1**: model emits `read_file` and gets the content — **first-hop recovery fires**, contradicting the 2026-05-17 "0/3 recovery" claim
- **turn 2**: model produces a `FinalAnswer` asking the user *"Please provide me with the new content for this line."* — the user's original prompt already specified the replacement (`'bar'`), but the model treats the read result as the end of the task and delegates the actual edit back

`tool_calls: 1` (`read_file`), `stop_reason: FinalAnswer`, `guards_by_kind: {"read_before_write": 1}`. Partial recovery, not full. Predicate flip on `12-edit-file-read-or-write` from `max_tool_calls=0` to `min/max_tool_calls=1, must_call_any_of=["read_file"]` captures the new shape.

**Root cause (of the second-hop gap)**: Composing a *different* tool call (`edit_file`) after consuming a tool *result* (`read_file`'s output) is multi-turn-chain territory. `neo-llm-bench` measured 0% BFCL multi-turn at this model size (Q8_0, temp=0). The 1.5 B model can do one tool hop with a deterministic reasoning chain — what it can't sustain is a chain that *follows up on a tool result* and emits a *different tool's composition*. The placeholder recovery (11) worked end-to-end because the second hop was the *same tool with a different body*, which is single-hop territory the chain can hold. Edit-after-read requires the chain to span a tool boundary and a result digest, and that's the floor.

The model's final answer ("provide me the new content for this line") is the same instruction-tuned "delegate tool work to the user" pattern as the original entry's failure mode, just one step further into the recovery chain. The 1.5 B model's "echo and stop" instinct fires whenever the chain runs out of confident next-step reasoning, regardless of how deep into recovery we are.

**Fix / takeaway**: Two takeaways, neither acted on in this commit:

1. **First-hop recovery is achievable through harness-side nudges.** Mirroring the dispatch path's `failure_memory_note` injection at the guard `continue` path is a small change (~10 lines + a hint function with a single `Some`) that converts a 0% recovery rate into a 100% first-hop rate on this fixture. The principle generalizes: **every place the agent loop short-circuits without going through `dispatch`, audit whether the missing affordances (`coach::coach`, `failure_memory_note`) are load-bearing.** Today, only `read_before_write` benefited; future guard kinds that target an actionable refusal pattern (rather than a terminal exit or a "just stop" steer) opt in by adding their hint to `guard_failure_memory_note`.

2. **Second-hop closure is feature work, not harness scaffolding.** Two architectural options for a future cycle:
   - *(a) Post-recovery-read system note.* After a recovery-read succeeds (i.e., a `read_file` that was preceded by a `read_before_write` fire), push a system note: "You have read the file. Now perform the original edit." Mechanically cheap; risks expanding the system-note budget at 8192 ctx; needs tracking "this read was a recovery read" through one extra piece of state in the agent loop.
   - *(b) Auto-read on guard refusal.* Replace "refuse and coach the model to read" with "refuse, *automatically* read the file ourselves, and replay the original edit_file call with the read content in context." Converts the two-hop chain into a one-hop chain at the harness layer entirely. Bigger change; aligns with the project's "make the harness smarter than the model" thesis; might require care that the auto-read doesn't itself trip other guards (e.g. cold_read if the path basename isn't in user input — but it would be, because the model just named it in the edit_file args).

   *(b)* feels more aligned with the project posture. Parked for now: fixture 12 captures the partial-recovery shape, so when *(b)* lands, the predicate flip from `tool_calls=1` to `tool_calls=0` (auto-read absorbs the read into the harness, model only sees one successful edit_file) is the next intended signal.

3. **The `0% multi-turn floor` from `neo-llm-bench` is load-bearing as a *design* constraint, not just a *measurement*.** Every harness feature that requires the model to bridge a tool-result-to-different-tool boundary is going to fight this floor. The thesis "make the harness smarter than the model" reads, in this light, as "the harness should absorb multi-turn chains so the model only ever has to do single-hop work." Auto-read on guard refusal is one application of that thesis; future guards should be designed similarly.

**Predicate-design note**: Fixture 12 demonstrated the orthogonality of the new guard-fire predicate set in practice. Predicate flip across versions:
- Pre-wiring (anchor): `max_tool_calls=0`, `must_fire_guards=["read_before_write"]`, `max_guard_fires=1`
- Post-wiring (this entry): `min/max_tool_calls=1`, `must_call_any_of=["read_file"]`, `must_fire_guards=["read_before_write"]`, `max_guard_fires=1`

The `must_fire_guards` and `max_guard_fires` predicates are *invariant* across both versions — same kind, same count, every rep. What flipped is the recovery-hop-count predicates, which is the right axis: kind predicates pin the harness behavior (which guard fires), count predicates pin the model behavior (how far the chain reaches). The 2026-05-17 commit landing the predicates and yesterday's commit using them in anger together vindicate the design split.

**Affected files**: `src/agent/coach.rs` (`guard_failure_memory_note` function + 5 unit tests), `src/agent/mod.rs` (wiring at the read_before_write and cold_read guard `continue` branches; cold_read inherits the wiring as a no-op so new kinds opt in through the coach function), `bench/tasks/12-edit-file-read-or-write.toml` (predicate flip), `bench/baselines/main/12-edit-file-read-or-write-rep[0-2].jsonl` (new traces; old traces from the anchor version replaced), `bench/baselines/main/summary.json` (fixture-12 outcomes replaced, `n_outcomes` stays at 36).

---

### [2026-05-17] Auto-read on `read_before_write` refusal — option (b) lands and the second-hop gap closes

**What happened**: Closed the partial-recovery gap documented in the previous two entries by landing option (b) from the parked-architectural-decisions list: auto-read on guard refusal. When `read_before_write` fires against an `edit_file`/`write_file` on an unread target, the harness now performs a bounded `read_file` *itself*, records the call with `origin = SyntheticGuardRecovery { guard: "read_before_write" }` (schema v3), surfaces the content to the model as a system note alongside an explicit retry instruction, and lets the loop iterate. The model's next turn sees the content already in scope and only needs to compose `edit_file` — the single hop the 1.5 B model CAN sustain.

Two probes informed the choice. First an (a) measurement probe ran the simpler design alternative: keep refusing the dispatch and just push a system note ("you have read the file; now perform the original edit") after the model performs its own recovery read. Across 10 reps at temp=0, (a) hit **7/10 task success** at 8400–10780 tokens / 14–34 s wall — materially better than Reviewer 3's predicted 1–3/10, exactly at the stop-rule threshold, but at 2–3× the token cost of the pre-this-work baseline and leaving a 3/10 dedup-loop failure mode where the model retries the same recovery `read_file` 3× post-refusal. The (a) branch was preserved on the `phase0-a-probe` git branch as a frozen reference; the code itself was discarded.

The cost regression motivated proceeding to (b) despite (a) hitting the success threshold. Auto-read shape across 3 reps at temp=0, structurally bit-deterministic: 1 model-emitted `edit_file` (refused) → 1 synthetic `read_file` with `origin=SyntheticGuardRecovery` (succeeded) → 1 model-emitted `edit_file` (succeeded) → `FinalAnswer`. **3/3 task success at 4480 total_tokens / 4.8–11 s wall** — ~50% of (a)'s token cost, ~30% of (a)'s wall time, dedup failure mode eliminated. The remaining variance is the cold-cache effect documented 2026-05-15 (rep 0 burns 119 extra prompt tokens; reps 1+ bit-identical).

**Root cause** (of why (b) works where (a) didn't reliably): (a) still requires the model to bridge a *tool-result-to-different-tool* boundary on the recovery turn (`read_file` result → emit `edit_file`). That's exactly the BFCL multi-turn floor that `neo-llm-bench` measured at 0% on this model size, and the 7/10 in (a) is the noise of the model occasionally lucking into that composition rather than reliably making it. (b) removes the boundary entirely by absorbing the read into the harness layer: the model only emits `edit_file`, observes the content via a system note (no tool-result-to-different-tool composition needed), and never has to perform the failure-prone state transition.

**Fix / takeaway** — five architectural primitives landed together:

1. **Schema v3 with `ToolOrigin` provenance.** New `ToolCall` / `ToolResult` field `origin: Option<ToolOrigin>`. Two variants today: `Model` (default — omitted from the wire when `None`) and `SyntheticGuardRecovery { guard: String }` (carries the kind of guard that drove the auto-recovery, so future synthetic sources opt in by adding their kind). Bumped `SCHEMA_V` 2 → 3 even though the additive-optional-field convention would have permitted staying at v2 (cf. the v1→v2 entry). The reason: a documented semantic-capability change ("the harness can now author tool calls") warrants version-level visibility so consumers can distinguish "field absent = model-originated" from "field absent = old trace." Backward compat verified: 36/36 of the pre-v3 canonical baseline replays green against the v3 binary (three round-trip tests pin the contract).

2. **`try_auto_read_for_rbw` in `src/agent/mod.rs`.** ~80-line free function. Takes disjoint references (`recorder`, `tools_by_name`, `cache`) rather than `&mut Session` to keep the borrow checker happy. Records `ToolCall` *first* with `origin=SyntheticGuardRecovery`, dispatches `read_file`, records `ToolResult` with the same origin on both success and failure (balanced traces). Bounded by (a) the existing `read_file` tool-layer 24 KB byte cap (no duplicated logic) and (b) a new 800-line ceiling guarding pathological minified/generated files. Returns `Some(call)` for the caller to drop into a system note via `auto_read_recovery_note`; returns `None` on dispatch error or line-cap violation so the caller falls back to the existing refusal + `guard_failure_memory_note` shape.

3. **Recursion invariant by construction.** The synthetic dispatch is always `read_file`; `read_before_write` only fires on `write_file` / `edit_file`. Therefore the auto-read cannot itself trigger another auto-read — single-hop guaranteed without any runtime depth-counter. Documented at the function level; future synthetic sources that target the same tool surface they recover from will need explicit depth tracking.

4. **Counterfactual visibility.** The refusal note is still pushed as a `tool_result` for the blocked `edit_file`'s id (the conversation protocol requires a response per assistant tool_call). The auto-read content is in a *separate* system message after the refusal note, so the model sees "your edit was refused" → "the harness read the file for you, here's the content" → retry. The trace records the guard fire (`Guard` event) AND the harness-injected calls (`ToolCall`/`ToolResult` with origin set), so replay can audit both that the guard fired and that the harness intervened — not "the read succeeded magically."

5. **Provenance-aware bench predicates.** `Summary` gained `synthetic_tool_calls: u32` + `synthetic_tool_calls_by_name: BTreeMap<String, u32>` (subset of `tool_calls` / `tool_calls_by_name`). `TaskExpect` gained `must_have_synthetic_calls: Vec<String>` + `must_not_have_synthetic_calls: Vec<String>`. Pre-v3 traces (with no `origin` field) produce empty synthetic counts, so `must_have_synthetic_calls` is fail-closed against pre-v3 traces — by design, the contract requires a v3 emitter to satisfy. Fixture 12 uses both new predicates: `must_have_synthetic_calls = ["read_file"]` (positive assertion the auto-read fired) + `must_not_have_synthetic_calls = ["edit_file", "write_file"]` (defensive — no future change should synthesize a mutating tool).

**Architectural distinction worth surfacing** (Reviewer 3's point): fixture 12 post-(b) no longer measures the model's ability to recover from `read_before_write`. It measures the harness's auto-read orchestration plus the model's ability to consume already-materialized context. That's a *different capability layer* than what the fixture was originally for, and it's a deliberate posture commitment: the project thesis is "make the harness smarter than the model," and (b) commits harder to that. The pre-(b) "can the model recover?" question now lives at `bench/archive/12-edit-file-read-or-write-pre-auto-read.toml` — a regression canary that passes if and only if the auto-read regresses. Inverted predicate semantics.

**Generalization budget**: Auto-read is *not* a generic "if guard fires, synthesize what the model would have done" pattern. It works for `read_before_write` because the guard already names the exact path the model needs, so the harness has unambiguous information about what to recover. The same approach doesn't trivially extend to e.g. `dedup` (no obvious recovery action) or `cold_read` (refusal is *correct* — the recovery is "answer the user directly," not "do the read anyway"). When a second guard wants similar treatment, the right move is to lift `recovery_action: Option<SyntheticTool>` out of the `read_before_write` branch — premature now.

**Predicate-design observation**: The pre-(b)/post-(b) predicate flip captures a clean three-axis split:

| Axis | Pre-(b) | Post-(b) |
|---|---|---|
| Total calls | `min/max_tool_calls = 1` (just the model's recovery `read_file`) | `min/max_tool_calls = 2` (synth `read_file` + model `edit_file`) |
| Model behavior | `must_call_any_of = ["read_file"]` (model emits the recovery read) | `must_call_all_of = ["edit_file", "read_file"]` (both appear; model emits edit, harness emits read) |
| Provenance | (no provenance predicates needed — pre-v3) | `must_have_synthetic_calls = ["read_file"]` + `must_not_have_synthetic_calls = ["edit_file", "write_file"]` |

The orthogonality of the three is what makes the regression-canary archive fixture work: it pins the *pre-(b) shape* via the first two axes only, deliberately omitting provenance assertions. If a future change regresses (b), the archive fixture's `tool_calls = 1` and `must_call_any_of = ["read_file"]` start passing again while the canonical fixture's `must_have_synthetic_calls = ["read_file"]` fails. Two fixtures flip in opposite directions — unambiguous signal.

**Side-fix surfaced during baseline regeneration**: Fixture 04 (`length-truncation`) had `max_wall_ms = 60000` calibrated against the original dev rig where the workload measured ~12 s. On the current rig (older Metal generation, pre-M5/A19 compatibility mode per llama-server output) the same workload measures 50–70 s. Rep 2 of the first re-baseline hit 71 s and failed the cap. Bumped to 90000 ms with a comment naming the rig variance — the fixture's purpose is pinning the `length` guard fire, not benching wall time, so a generous cap is appropriate. The narrow 60 s window was a calibration artifact, not a meaningful regression detector.

**Affected files**: `src/obs/recorder.rs` (`ToolOrigin` enum + `origin` field on `ToolCall`/`ToolResult` + `SCHEMA_V` 2→3 + three round-trip tests), `src/obs/mod.rs` (re-export `ToolOrigin`), `obs/schema.md` (v3 section with provenance contract), `src/agent/mod.rs` (`AUTO_READ_LINE_CEILING` + `try_auto_read_for_rbw` + `auto_read_recovery_note` + read_before_write guard branch wiring), `src/bench/summary.rs` (synthetic counters + `must_have_synthetic_calls` / `must_not_have_synthetic_calls` checks + 7 unit tests), `src/bench/fixture.rs` (synthetic-call predicate fields + 2 tests), `bench/tasks/12-edit-file-read-or-write.toml` (post-(b) shape), `bench/tasks/04-length-truncation.toml` (wall cap recal), `bench/archive/12-edit-file-read-or-write-pre-auto-read.toml` + `bench/archive/README.md` (regression-canary contract), `bench/baselines/main/` (re-baselined 36/36).

> **Update 2026-05-17 (same day, third entry chain):** the 3/3-rep baseline from this entry was a warm-cache outlier. The auto-read system-note delivery (b-current) measures at 4/10 task success across cold-server reps-10 stress runs. The chained entry below covers the b-toolresult fix, the failed Phase-C prompt probe, and the Phase-B envelope codification that ultimately landed.

---

### [2026-05-17] b-current is 40% at scale, b-toolresult is 87%, Phase-C prompt fix regresses to 60% — Phase-B envelope codification ships

**What happened**: The prior entry's 3/3 baseline ("auto-read closes the second-hop gap") was a warm-cache outlier. A reps-10 stress on cold-restarted `llama-server` showed b-current (auto-read delivered as a system note) actually runs at **4/10 task success**, with 6/10 reps showing a new failure mode: the model receives the synthetic read content via system note, says *"Now I'll edit it"*, and then prose-writes the post-edit file content as if the edit happened — but emits no `edit_file` tool call. Identical "delegate-tool-work-via-prose" failure documented 2026-05-14 and 2026-05-17 first entry, now appearing on the recovery turn.

The lesson: **a 3-rep baseline characterizes server-prompt-cache-state more than it characterizes the stochastic envelope at temp=0.** The cold-cache vs warm-cache delta documented 2026-05-15 leaks into completion-side bytes (per 2026-05-16 fixture-11 lesson), and when the failure mode has two stable continuations the 3-rep window can land entirely on one branch by chance. Always reps-10 cold-server stress a recovery-fixture before committing the canonical baseline.

**Probe sequence + resolution**:

Three deliveries tried in order:
- **b-current (system note)**: 4/10. Truncated-branch model proses post-edit content.
- **b-toolresult (synthetic `tool_call`/`tool_result` pair)**: 26/30 = 87% across 30 stress reps. Four stable shapes:
  - `clean` (47%): synth read + model edit → FinalAnswer, 4276-4314 tokens, 3-12s wall.
  - `verify_read` (40%): synth read + model edit + post-edit verification read → FinalAnswer, 6299-6355 tokens, 9-15s wall.
  - `length_truncated` (10%): synth read + model writes `echo ... | bash -c 'sed -i ...'` snippets inside markdown code blocks → 2048 completion tokens → Length stop. New failure family — same prose-as-action root cause, different surface.
  - `no_call_fa` (3%): synth read + model prose-displays post-edit content in markdown → FinalAnswer with no edit_file emission. Same surface as b-current's truncated branch but rarer.

  The format change validated the hypothesis: chat-template "after-tool_result" continuation reliably elicits a tool_call (87% vs b-current's 40%). The remaining 13% is the BFCL multi-turn floor manifesting on the *which-tool* axis instead of the *tool-vs-prose* axis. The model picks a wrong tool channel (prose-bash code block or prose-display) in those reps.

- **Phase-C prompt rule probe** (negative-general framing per reviewer convergence — `*"Markdown code blocks in your reply only display text; they do not execute. To modify a file, emit a tool call — do not output a shell pipeline (e.g. sed, awk) as a code block in your reply."*`): 6/10 task success. The rule successfully eliminated the bash-loop AND verify-read shapes but **redistributed failure mass into no_call_fa** (3% → 40%). The model went from "I'll write sed commands" to "okay, I'll just display the new content and stop" — same prose-as-action failure family, different surface that the rule didn't catch. Reverted; prompt rules at 1.5 B parameters can suppress specific surfaces but cannot eliminate the underlying continuation-mode failure.

**Root cause** (of why prompt rules can't close the remaining 13%): the failure mode isn't "model doesn't know to call edit_file." The 87% of b-toolresult reps show the model *can* compose the recovery tool call. The 13% failure modes are the model probabilistically choosing wrong output channels — markdown for action, prose for display — at the precise tokenization position where the chat template would otherwise yield a `tool_call`. That decision is logit-level; system prompt rules influence the *distribution* but don't eliminate the long tail.

**Resolution (Phase B)**: codify the 87% envelope with compositionality preserved. Fixture 12 becomes a **guard-intervention characterization fixture** (new category, per Reviewer 3 framing), distinguished from the **task-success deterministic fixtures** (01–08, 11) that pin single deterministic shapes. The job of fixture 12 is now: "auto-read fires correctly + the model composes ≥1 tool call beyond the synthetic read." Predicate updates:

- `min_tool_calls = 2`, `max_tool_calls = 3` — admits clean + verify_read, rejects no_call_fa and length_truncated which have only the synthetic read at tool_calls=1.
- `min_model_tool_calls = 1` — **load-bearing compositionality predicate, new in this commit**. Without it, the must_have_synthetic_calls predicate would trivially pass on no_call_fa (synth read fires, model emits nothing, harness intervention succeeded but no recovery happened). Reviewer 3's key catch.
- `must_have_synthetic_calls = ["read_file"]`, `must_not_have_synthetic_calls = ["edit_file", "write_file"]` — provenance invariants.
- `must_not_call` keeps `bash` forbidden — real bash-tool dispatch is a different failure class than the prose-bash code blocks (those don't dispatch bash; they hit max_tokens in markdown text). Reviewer 3's second catch.
- `stop_reason` dropped — Length-truncated and no_call_fa reps both have valid `stop_reason` values; the `min_model_tool_calls` predicate is what rejects them.
- `max_total_tokens = 7000`, `max_wall_ms = 75000` — generous to admit the verify shape's 6355-token p95 and the length-truncated 69-second wall.

**Predicate design observation**: the kind × count × provenance three-axis split landed in the prior commit is now joined by a fourth implicit axis — **compositionality** — via `min_model_tool_calls`. The four-axis design is what lets fixture 12 distinguish "harness intervention succeeded" from "model genuinely composed a recovery action" without conflating them. Future intervention-characterization fixtures should plan for these four axes from the start.

**Fixture taxonomy** (new, surfaces in `bench/README.md`): bench fixtures now split into two categories:
- **task-success deterministic**: pin a single observable shape every rep (most fixtures). Failure = behavioral drift.
- **guard-intervention characterization**: pin the *invariants* of harness behavior across a stable multi-shape envelope (fixture 12 today). Failure = invariant violation OR distribution drift outside the documented envelope. The envelope itself is persisted at `bench/baselines/main/<fixture-id>-stress-envelope.json` so future regressions can compare.

The principle: when the model has multiple stable continuations at the BFCL multi-turn floor and the harness intervention is the load-bearing correctness layer, the right fixture is one that asserts the harness invariants and bounds the envelope — not one that pretends the model produces a single shape it doesn't.

**Stress envelope persisted**: `bench/baselines/main/12-stress-envelope.json` captures the 30-rep aggregate from three cold-server stress runs. Future regression checks: aggregate distribution should match within reasonable error; if `task_success_rate` drops below ~70% or a fifth shape appears, investigate.

**Strategic concern Reviewer 3 surfaced explicitly**: this commit moves fixture 12 from "task completion determinism" to "bounded intervention observability." Defensible (the harness IS what's correct here, not the model), but a real philosophical shift. Future contributors reading "fixture 12 passes" need to understand the meaning is *"the auto-read fired and the model did something resembling work,"* not *"the task completed deterministically."* The category note in the fixture comment header is the contract.

**What we did not do — and why**:
- Did not pursue b-strict (harness replays `edit_file` with captured args). Crosses the authorship boundary Reviewer 3 flagged on fixture 11's content-recovery rejection. Would likely give ~100% task success but at the cost of changing the harness's authorship semantics from "synthesize precondition affordances" to "replay model intent as harness action." Not worth it for 13% more task success.
- Did not iterate the prompt fix beyond the negative-general first attempt. Reviewer-3 explicit: "ship or bail." Adding a positive nudge ("after a tool result, your next step is usually another tool call") would be prompt magic — non-local effects and brittle.

**Affected files**:
- `src/agent/mod.rs` — b-toolresult delivery: replaces `auto_read_recovery_note` (system-note builder) with `synthetic_read_call_message` (fabricated assistant `tool_call(read_file)`); push the synthetic content as a paired `tool_result` instead of a system message. The old shape is fully removed, not toggled.
- `src/llm/types.rs` — no change (existing `ToolCall` / `FunctionCall` already public).
- `src/bench/summary.rs` — `model_tool_calls: u32` derived field on `Summary` (`tool_calls` minus `synthetic_tool_calls`) + `min_model_tool_calls` predicate check + 4 new tests.
- `src/bench/fixture.rs` — `min_model_tool_calls: Option<u32>` on `TaskExpect` + parse-test extension.
- `bench/tasks/12-edit-file-read-or-write.toml` — Phase-B predicates, category note in header, narrative of the three-delivery arc.
- `bench/baselines/main/` — re-baselined fixture 12 with b-toolresult traces (1 verify + 2 clean shapes out of 3 reps); full suite 36/36; `12-stress-envelope.json` added as the canonical 30-rep regression artifact.

---

### [2026-05-17] Why fixture 11 won't get an auto-recovery: closed as written rejection

**What happened**: After the auto-read landing on `read_before_write` (fixture 12), the obvious adjacent question was *"why not the same treatment for `write_file`'s placeholder rejection on fixture 11?"* — the model emits `// TODO: implement` as a function body, the tool layer rejects with the "placeholder" honesty guard, and the model recovers 9/10 reps but loops into Dedup the 10th (lessons.md 2026-05-16). An auto-stub recovery (harness rewrites the placeholder to a typed-stub from the function signature) would in principle close the 1/10 gap.

Closed without empirical probe. The categorical reason: **read-before-write recovery works because the model lacked *information* the harness could safely provide; placeholder writes are different because the model has the *affordance* but emits *low-integrity content*.** Auto-stubbing converts "agent failed honesty/integrity check" into "harness silently manufactures acceptable content" — a categorically different kind of intervention than satisfying a precondition, and one that changes the authorship semantics in a way read-recovery does not.

The (b) auto-read satisfies a precondition (read the file before modifying); the model's *intent* is preserved verbatim, just executed after the precondition is discharged. An auto-stub would *originate* content the model never specified, attributed to the model. That's a boundary that should hold regardless of whether the resulting content happens to compile.

**Fix / takeaway**: no code change. Recorded here as doctrine: *"the harness synthesizes preconditions (information the model needed), not substance (content the model would have authored)."* The systemic-safety column of the audit rubric in `bench/PREDICATES.md` is the rule-encoded form of this principle.

**Re-open condition**: this door can be re-opened if the harness gains a *mechanically-derivable transformation* from placeholder content to non-placeholder content with preserved provenance — e.g., a typed-stub generator that produces `unimplemented!()` from a Rust function signature, attributed in the trace as `origin = SyntheticGuardRecovery { guard: "placeholder_rejection" }`, with the model's original `// TODO` retained in the conversation as counterfactual visibility. The transformation must be (a) mechanical (no model judgment), (b) provenance-preserving (clear in the trace that the stub came from the harness), and (c) systemically safe (doesn't generalize to "harness silently fixes any rejected output"). Until those three properties can be jointly demonstrated for a candidate transformation, the door stays closed.

**Affected files**: none. Documentation-only closure.

---

### [2026-05-17] Guard-branch affordance audit — `length`'s malformed-args family is empirically empty on 1.5 B; disposition resolves to documented no-op

**What happened**: Tier 2 of the post-Phase-B plan was the guard-branch affordance audit — apply the rubric in `bench/PREDICATES.md` (recoverable × deterministic × local-safe × systemic-safe) to each guard kind and decide a disposition. Most kinds were already settled by doctrine: `dedup` / `turn_cap` are safety brakes (PREDICATES.md doctrine rules them out as auto-recovery candidates); `cold_read` is conversational (refusal already steers correctly); `write_pressure` is structurally unreachable on this model (pinned by fixture 10 since 2026-05-16). The open question was `length` — the rubric marked it "maybe" pending probe data.

The probe-first discipline from Reviewer 3 ruled out any speculative recovery design. The post-Phase-B revised plan defined three plausible failure families to disambiguate:
- **2.2.a malformed-args**: model emits a tool call whose JSON args get cut off mid-emission.
- **2.2.b semantic-derailment**: model writes meandering prose, runs out of tokens.
- **2.2.c clean-cutoff**: model is producing structured output that legitimately exceeds `max_tokens`.

Two of the three families turned out to already be characterized in committed fixtures:
- **2.2.c clean-cutoff** is what fixture 04 (`length-truncation`) has been pinning since 2026-05-15: prompt asks for "count 1 to 2000," model produces numbered lines until `max_tokens=2048` is hit, no tool calls, `Length` stop reason.
- **2.2.b semantic-derailment** is what fixture 12's `length_truncated` envelope shape captures (3/30 stress reps in `12-stress-envelope.json`): post-recovery the model writes `echo ... | bash -c 'sed -i ...'` blocks ad infinitum until `max_tokens` hits.

Only **2.2.a malformed-args** lacked empirical evidence. A targeted probe fixture (`13a-length-write-file-bulk`) was designed to elicit it: the prompt asks the model to `write_file` 100 lines of verbose content, expecting either (i) partial tool args truncate (malformed-args family is real) or (ii) the model emits short args and never gets close to truncation (family is empirically empty).

**Result across 10 cold-server reps, bit-identical at 2785 tokens / 1 tool call**: the model emits a `write_file` call with a *single line* of content (the literal example sentence from the prompt) and then *claims in prose* that it wrote 100 lines. Completion tokens never exceed ~100 per turn — orders of magnitude below the 2048 cap. **The malformed-args family is empirically empty on `qwen25-1.5b-instruct` at temp=0.** The model has a strong prior toward abstracting long inputs to short tool args, with the gap papered over by a confident (and false) prose claim of completion.

**Root cause** (of why the family is empty): two structural facts of the 1.5 B model at this scale compound. (1) The model rarely produces tool args > ~500 chars even when explicitly asked. (2) The model would rather lie in prose about the work than produce verbose arguments. Both behaviors are documented adjacent failure modes (the 2026-05-14 "polite apology" pattern; the 2026-05-17 first-entry "prose-the-edit" pattern). Together they ensure tool args never grow large enough to be truncated by `max_tokens`. The malformed-args failure family requires a model behavior that doesn't exist at this scale.

**Fix / takeaway** (length disposition = documented no-op):

| Family | Empirical status | Plausible recovery | Disposition |
|---|---|---|---|
| 2.2.a malformed-args | Empirically empty (this probe) | N/A — family doesn't manifest | No-op (nothing to recover) |
| 2.2.b semantic-derailment | Observed at 10% in fixture-12 envelope | "Retry with tighter `max_tokens`" produces shorter prose loop, doesn't escape derailment | No-op (no clean recovery) |
| 2.2.c clean-cutoff | Observed in fixture 04 | Mid-token resumption requires reliable continuation, which the model isn't | No-op (no clean recovery) |

None of the three families admits a recovery shape that meaningfully improves on the current handling (emit Guard{length}, push "be more concise" note for next user turn, break). The current code is the right disposition. The disposition is **encoded as a documented no-op** by:

- The renamed test `guard_failure_memory_silent_for_terminal_guards` in `src/agent/coach.rs` with explicit doctrine cross-reference + the empirical record of the three-family probe.
- The renamed test `guard_failure_memory_silent_for_safety_brake_guards` covering `dedup` and `write_pressure` per PREDICATES.md doctrine.
- The fixture 13a baseline traces themselves, which lock the empirically-empty-malformed-args shape as a regression anchor — if a future model rev produces long tool args, the fixture's `min_tool_errors = 0, max_tool_errors = 2` accepts the new shape but the trace token count will visibly change.

**Two side-findings worth flagging** (out of scope for length recovery; recorded here so future work can pick them up):

1. **The model lies about long writes.** Fixture 13a's prose claims completion of 100 lines after writing 1 line. This is a *content-honesty* issue, not a length-truncation issue. Adjacent to fixture 11's placeholder rejection (which catches certain integrity failures via the tool layer) but distinct. The current honesty-guard set doesn't catch "claimed work that wasn't done" because the model's *tool call did succeed* (it wrote a valid file, just not the requested content). Closing this gap would require comparing the prose claim to the tool output — a meta-honesty guard. **Parked.** Re-open if a fixture surfaces it as a load-bearing failure.

2. **`bench/PREDICATES.md`'s anti-pattern section now has empirical backing.** The hypothetical "tight fixture-12 predicate set" the doc warns against would have, in addition to failing on fixture-12's envelope, ALSO mischaracterized fixture 13a (it would have asserted some specific tool-arg shape and failed when the model abstracted). The anti-pattern is real across at least two fixtures — useful prior for future fixture authors.

**Affected files**:
- `bench/tasks/13a-length-write-file-bulk.toml` — new probe fixture; characterization-not-gating predicates per the PREDICATES.md taxonomy. Tagged in its comment header as a probe.
- `bench/baselines/main/13a-length-write-file-bulk-rep[0-2].jsonl` — canonical 3-rep baseline (10-rep stress also exercised under `bench/runs/phase3-length-probe-13a/` but gitignored).
- `bench/baselines/main/summary.json` — re-baselined; full suite now 39/39 (13 fixtures × 3 reps).
- `src/agent/coach.rs` — renamed `guard_failure_memory_silent_for_unreachable_guards` → `guard_failure_memory_silent_for_safety_brake_guards`; both renamed and `guard_failure_memory_silent_for_terminal_guards` got beefed-up doctrine-cross-reference comments graduating their no-op status from "current state" to "documented per audit rubric." Reviewer 1's catch: `write_pressure` specifically needed promotion from "likely no-op" to "documented no-op."
- `lessons.md` — this entry.

---

### [2026-05-17] Project close-out: paused indefinitely pending luxe sync

**What happened**: After landing the doc refresh that closed out the post-Phase-B engineering arc at the surface layer (README + CLAUDE + ARCHITECTURE + agents.md all in sync with the current state), the project reached a natural stopping point. **micro-mind is paused indefinitely pending new primitives in [`michaeldtimpe/luxe`](https://github.com/michaeldtimpe/luxe).** No further harness work is planned in this repo until luxe ships something worth porting to the 1.5 B / Rust target.

**State at the close** (anchor for the next reopen):

- **Code**: 148/148 unit tests passing. Schema v3 with `ToolOrigin` provenance + `min_model_tool_calls` compositionality predicate. Auto-read on `read_before_write` via `try_auto_read_for_rbw` + `synthetic_read_call_message` (b-toolresult delivery shape). Every other guard kind has a documented no-op disposition with doctrine cross-reference.
- **Bench surface**: 13 fixtures × 3 reps = 39/39 canonical baseline replay (gating). Migration-check 60/60 (gating). Four-axis predicate framework (kind × count × provenance × compositionality). Fixture taxonomy split (task-success-deterministic vs guard-intervention-characterization).
- **Foundation docs**: `bench/PREDICATES.md` (predicate design + audit rubric), `bench/STRESS-PROTOCOL.md` (reps-10 cold-server discipline), `bench/audits/2026-05-17-archive-vs-main.md` (0 hard regressions over two months of work).
- **Empirical anchors**: `bench/baselines/main/12-stress-envelope.json` (30-rep envelope for fixture 12); `bench/tasks/13a-length-write-file-bulk.toml` (length-probe baseline locking the malformed-args-empirically-empty finding); `bench/archive/12-edit-file-read-or-write-pre-auto-read.toml` (regression canary for pre-(b) recovery shape).
- **Narrative**: this is `lessons.md`'s sixth 2026-05-17 entry. The five preceding entries cover the full arc end-to-end — original auto-read landing → b-current 4/10 disaster → b-toolresult 87% fix → Phase-C reverted prompt probe → Phase-B envelope codification → Block-1-thru-4 doc framework + length audit.

**Why the pause**: micro-mind takes the harness lessons from luxe and re-targets them at the 1.5 B model in Rust. The dependency is one-directional (luxe → micro-mind). The current micro-mind state already encodes every luxe primitive that's been ported to date; further ports require luxe to land something new worth porting. No internal work is blocked on anything external; the work is just done.

**Tier 4 explicitly skipped**. The post-Phase-B revised plan had two optional research probes parked at Tier 4: a 30-rep (a) probe with stated null hypothesis (narrative-completeness for the "instruction tuning alone plateaued" claim), and a cold-cache delta investigation (mechanism behind the documented prompt_tokens drift). Neither is load-bearing for shipping; both were designed and explicitly not run. The plan exists in conversation history; the items are not encoded as tracked work.

**Reopen condition**: when luxe lands a primitive worth porting. Specific reopen-shaped questions:

- *"luxe added a new tool / guard / recovery — can we port it?"* → yes, follow the doctrine in `bench/PREDICATES.md` (audit rubric → probe-first if recoverable → guard-intervention-characterization fixture if envelope is multi-shape).
- *"luxe changed its agent loop in shape X — should we mirror?"* → depends on whether the change addresses a 1.5 B failure mode (port) or a 35 B MoE failure mode (probably don't port; harness divergence is fine when the model class differs).
- *"luxe deprecated primitive Y — does that affect us?"* → probably not, but worth checking `agents.md` and `ARCHITECTURE.md` for cross-references.

**Non-reopen conditions** (do not resume on these alone):

- *"can we do Tier 4 now?"* → only if the user explicitly asks. Don't suggest proactively.
- *"can we run the bench again?"* → diagnostic, not resumption. Answer yes, point at the canonical baseline.
- *"can we add fixture X for behavior Y?"* → depends on Y. If Y is new model behavior observed in the field, yes (the bench surface is the right home). If Y is speculative, no.

**Affected files**: README.md (status header), CLAUDE.md (status block at the top so it loads into every Claude Code session immediately), this entry, and a durable project memory at `~/.claude/projects/-Users-michaeltimpe-Downloads-micro-mind/memory/project_state_2026-05-17_paused.md` (the latter for cross-session continuity outside the in-repo docs).

---
