# Bench stress protocol

How to characterize a fixture's stochastic envelope before locking
its predicates. Required when adding a guard-intervention
characterization fixture (see `bench/PREDICATES.md`); strongly
recommended for any fixture exercising a recovery path that the
harness influences.

The protocol exists because of one specific empirical disaster
documented in `lessons.md` 2026-05-17 (third entry): a 3-rep
canonical baseline for the (b) auto-read showed 3/3 task success;
a subsequent reps-10 cold-server stress showed 4/10. The 3-rep
window had landed entirely on one branch of a 2-shape envelope by
chance. The lesson: **a 3-rep baseline characterizes prompt-cache
state, not the stochastic envelope at temp=0.** Don't repeat it.

## Three phenomena to distinguish

A fixture that doesn't produce bit-exact output every rep is *not*
simply "flaky." There are three distinct phenomena that can produce
inter-rep variance, and they demand different responses.

### 1. Stochasticity (sampling-driven)

Temperature > 0 sampling produces genuinely random output. Different
reps draw different tokens.

**Not applicable to this project.** `micro-mind` pins `temperature =
0.0`, `top_p = 1.0`, `seed = 42` (see `CLAUDE.md`'s single-model
policy). If a fixture produces inter-rep variance, it is *not* from
stochasticity. Move to the next category.

### 2. Cache effects (server-state-dependent)

`llama-server`'s prompt cache changes across requests. The first
request after a cold start re-tokenizes the system prompt + tool
definitions from scratch; subsequent requests hit the cache. The
*model output* is bit-identical within a session for the same prompt,
but `usage.prompt_tokens` and (occasionally) completion-side bytes
drift by ~20-150 tokens depending on cache occupancy.

Documented across:
- `lessons.md` 2026-05-15 — first observation, "~20 tokens of usage accounting noise."
- `lessons.md` 2026-05-16 (fixture 11) — completion-side bytes can leak the cache state too, producing different stable shapes on different runs.
- `lessons.md` 2026-05-17 (third entry) — b-current's 3/3 warm-cache baseline vs 4/10 cold-server reality. This is the canonical example.

**Detection**: variance in `total_tokens` of ≤ ~150 across reps at
otherwise-identical trace event shapes is cache effects. Variance in
the *number or kind of tool calls* across reps is not cache effects
alone — see structural multimodality below.

**Mitigation**: stress runs must use cold-restarted `llama-server`
between reps (or at least between groups of reps) to characterize the
true envelope rather than a warm-cache subset. See the protocol below.

### 3. Structural multimodality (genuinely distinct shapes)

The most subtle phenomenon and the one this protocol most directly
guards against. At temperature 0, the model's reasoning chain is
deterministic *given the prompt and cache state*. Cache state can
change between reps. Sometimes those changes propagate into the
chain's branching decisions, producing *different but each
internally-deterministic* shapes.

Fixture 12's b-toolresult envelope is the case study: four stable
shapes (clean, verify_read, length_truncated, no_call_fa), each
bit-exact within reps that land on it, with stable proportions
across 30 cold-server reps.

**Detection**: when two or more reps produce distinct trace shapes
(different tool sequences, different stop reasons) but each shape is
bit-identical to other reps that land on the same shape, the fixture
has a structurally multimodal envelope. The number of shapes is
*discovered* by stress, not assumed.

**Mitigation**: characterize the envelope (this protocol), then
write predicates that lock invariants and bound the envelope rather
than asserting a single shape (see `bench/PREDICATES.md`'s
*Anti-pattern: precision that encodes model determinism*).

### Why the three distinction matters

A naive read of "fixture 12 has inter-rep variance" could compress
to "the model is non-deterministic, work around it." That's wrong on
all three axes:

- Stochasticity: there is none. The variance is not from sampling.
- Cache effects: present but bounded (~150 tokens); they do not
  account for the 4-shape envelope.
- Structural multimodality: this is the actual cause, and the right
  response is empirical (characterize) plus design (predicate sets
  that admit the envelope), not architectural (force determinism).

Future readers who don't internalize this distinction will likely
reach for prompt rules or seed tweaks (the wrong tool) before
reaching for envelope characterization (the right one).

## The protocol

### When to run a stress

**Required**:
- Adding a new guard-intervention characterization fixture.
- Changing the harness's behavior on a guard branch that an existing
  characterization fixture covers.
- Re-baselining after a change that could affect any recovery path
  (schema changes that affect provenance, dispatch-path changes,
  system prompt edits).

**Strongly recommended**:
- Any new fixture exercising a recovery path the harness influences
  (placeholder rejection, guard refusal, length-truncation handling).
- Before promoting a fixture from `bench/tasks/` to gating CI status
  if it's been baseline-captured at < 10 reps.

**Not required**:
- Task-success deterministic fixtures with no recovery affordance
  (fixtures 01–08 in their current form). The 3-rep baseline is
  adequate when the chain is genuinely deterministic.

### How to run a stress

```bash
# Cold-restart llama-server before the stress run.
pkill -f llama-server
sleep 3

# 10 reps minimum. 30 reps preferred for new characterization fixtures
# (the threefold sample gives stable shape proportions).
./target/release/bench-run \
  --filter <fixture-id> \
  --reps 10 \
  --bin ./target/release/micro-mind \
  --out bench/runs/<fixture-id>-stress-<timestamp>
```

For new characterization fixtures, repeat the 10-rep run **three
times** with `pkill -f llama-server` between each. The cross-run
variance is the cache-effects signal; within-run variance is the
structural-multimodality signal. 30 reps total.

### Abort criteria

Bail out and investigate before completing the run if:

- **>50% of reps are non-stable-shape failures.** Each shape should be
  bit-identical across the reps that land on it (within ~150 tokens
  for cache-effects noise). If half the reps produce one-off shapes
  with no stable repeats, the fixture's failure mode is itself
  non-deterministic — the fixture isn't characterizable and reps-10
  won't help. Stop at rep 5; investigate.
- **A trace event indicates infrastructure failure** (e.g., HTTP
  errors from `llama-server`, dispatch panics in `micro-mind`). The
  stress will be invalid; fix the infrastructure issue first.
- **Reps are >5× slower than the fixture's documented max_wall_ms.**
  Indicates a degraded server state. Restart and rerun.

Do NOT bail out for:

- Inter-rep variance in `total_tokens` ≤ 150. Cache effects.
- Two or three reps landing on a shape you didn't expect, *if* each
  shape is internally consistent and the predicates still hold. This
  is the envelope being characterized; finish the run.
- A single rep exceeding `max_wall_ms` by a small margin (< 2×).
  Cap calibration issue, not a fixture incoherence. Recalibrate after
  the run.

### Persisting the envelope

For characterization fixtures, persist the aggregate stats from the
stress run as a versioned artifact:

```
bench/baselines/main/<fixture-id>-stress-envelope.json
```

Fields the artifact should contain (see
`bench/baselines/main/12-stress-envelope.json` for the canonical
example):

- `fixture` — the fixture id.
- `category` — `guard-intervention-characterization` for any artifact
  produced by this protocol.
- `binary_state` — the commit SHA or branch state of the binary used
  for the stress.
- `reps_total`, `reps_per_run`, `runs` — sample size and structure.
- `cold_server_restarts_between_runs: bool` — `true` per this
  protocol.
- `sampling` — `temperature`, `top_p`, `seed`, `max_tokens`,
  `repeat_penalty`.
- `shapes_observed` — histogram of shape counts.
- `shape_descriptions` — one-line natural-language description of
  each observed shape.
- `task_success_count`, `task_success_rate` — for fixtures where
  task completion is well-defined.
- `tokens` — `p50`, `p95`, `min`, `max`.
- `wall_ms` — `p50`, `p95`, `min`, `max`.
- `comparison` — earlier mechanism rates if the fixture has a
  development arc worth documenting (e.g., fixture 12's comparison
  block shows pre-this-work / a-probe / b-current / b-toolresult /
  Phase-C-reverted rates).
- `notes` — free-form, especially the *why this is/isn't a
  task-success fixture* framing if the category isn't obvious.

Future regression checks should re-run the stress and diff against
the persisted envelope. **Acceptable drift**:
- Shape proportions within ±10 percentage points of the persisted
  histogram.
- Token p50 drift ≤ 5%, p95 drift ≤ 10%.
- New shape that satisfies all fixture predicates (the envelope is
  characterized at the time of capture; new shapes within the
  predicate envelope are legitimate evolution).

**Investigate**:
- Shape proportions shifting by >10 percentage points (potential
  drift in cache effects or model behavior).
- New shape that *fails* a fixture predicate (regression — what
  changed?).
- Task success rate dropping below the persisted rate by >5 points.

## Don't over-specify the expected envelope size

A fixture may have a 2-shape envelope, a 4-shape envelope (fixture
12's case), or more. The protocol is for *discovering* the envelope,
not validating an assumed shape count. Reps-10-cold-server is a
discipline for finding the shapes; the number you find is the answer,
not a target.

If a future fixture's envelope grows beyond 5 shapes, that's likely
a signal the fixture is exercising too many concerns and should be
split — but discover that empirically, not by enforcing a shape-count
cap in this document.

## Related artifacts

- `bench/PREDICATES.md` — how to design predicate sets that admit a
  characterized envelope without overfitting.
- `lessons.md` 2026-05-15 — the original cache-effects observation.
- `lessons.md` 2026-05-16 — fixture-11 completion-side cache leakage.
- `lessons.md` 2026-05-17 (third entry) — the b-current 3/3 → 4/10
  warm-cache disaster that motivated this protocol.
- `lessons.md` 2026-05-17 (fourth entry) — the 30-rep b-toolresult
  envelope characterization that demonstrates the protocol working.
