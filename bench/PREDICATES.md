# Bench predicate design

How fixture expectations in `bench/tasks/*.toml` express invariants
without overfitting to the model's stochastic envelope. The framework
this document encodes was extracted from the (b) auto-read work
landed 2026-05-17 — see `lessons.md` third and fourth entries for the
empirical history.

Two principles drive everything below. Internalize these before
designing a new predicate set.

> **Predicate precision is chosen to stabilize the invariant, not to maximize observability.**
>
> **Guard-characterization fixtures are expected to tolerate behavioral diversity inside a bounded envelope.**

The first principle prevents the common drift toward fixtures that
become surrogate model snapshots — every observable detail asserted,
none of them load-bearing for what the fixture is actually testing.
The second prevents the related drift of treating natural stochastic
multimodality as a failure to be suppressed. Both are easy to fall
into; both have specific empirical roots in fixture-12's evolution.

## The four predicate axes

Every predicate on a `TaskExpect` operates on one of four
**independent** axes. Designing a fixture means choosing which axes
are load-bearing for the invariant the fixture exists to pin, and
which are deliberately omitted.

### 1. Kind (which tools / guards appeared)

- `must_call_any_of: Vec<String>` — at least one of these tool names appeared.
- `must_call_all_of: Vec<String>` — every tool name in the list appeared.
- `must_not_call: Vec<String>` — none of these tool names appeared.
- `must_fire_guards: Vec<String>` — every guard kind in the list fired at least once.
- `must_not_fire_guards: Vec<String>` — none of these guard kinds fired.

Kind predicates answer *"which?"* — they're invariant to count.

### 2. Count (how many of each)

- `min_tool_calls: Option<u32>` / `max_tool_calls: Option<u32>`
- `min_tool_errors: Option<u32>` / `max_tool_errors: Option<u32>`
- `min_guard_fires: Option<u32>` / `max_guard_fires: Option<u32>`

Count predicates answer *"how many?"* — they're invariant to identity.

### 3. Provenance (who emitted the tool call)

Added in schema v3 alongside the auto-read landing.

- `must_have_synthetic_calls: Vec<String>` — every name appeared as at least one harness-injected call (`origin = SyntheticGuardRecovery`).
- `must_not_have_synthetic_calls: Vec<String>` — none of these tool names appeared as a synthetic call.

Provenance predicates distinguish model output from harness
intervention. A call to `read_file` from the model and a synthetic
auto-read both count toward `tool_calls`; provenance lets a fixture
assert *which one* fired without conflating them.

### 4. Compositionality (whether the model composed beyond the harness)

Added during Phase B (2026-05-17 fourth entry).

- `min_model_tool_calls: Option<u32>` — `tool_calls` minus `synthetic_tool_calls`, the count of model-emitted (non-synthetic) tool calls.

Compositionality predicates exist because provenance alone has a
trivial-pass failure mode: a fixture asserting
`must_have_synthetic_calls = ["read_file"]` would pass on the
no-call-FA shape where the synthetic read fires but the model emits
nothing. Compositionality forces *"the model also did its part."*

### Why four axes are independent

Each axis pins a different concern. The fixture-12 worked example
below shows them composing orthogonally. Mixing concerns into a single
predicate (e.g., trying to express compositionality via `min_tool_calls`
adjusted by synthetic-call accounting) creates fragile fixtures that
break on tangential changes.

## Fixture taxonomy

Bench fixtures fall into two categories with **different meanings of "passing."**

### Task-success deterministic

The model's behavior is reproducibly identical (bit-exact within a
session, structurally identical across sessions). Predicates pin the
*single observed shape*. A new shape is a regression.

Most fixtures: 01–08, 11. Use single-shape predicates without
compositionality assertions; the model's deterministic chain provides
the invariant.

### Guard-intervention characterization

The model exhibits a *stable multi-shape stochastic envelope* (not
random — distinct deterministic continuations at temp=0, selected by
prompt-cache state). The harness's intervention is the load-bearing
correctness layer, not the model's continuation. Predicates pin the
*harness invariants* and *bound the envelope*; specific shapes within
the envelope are not asserted.

Today: fixture 12 only. Companion artifact: `bench/baselines/main/<fixture-id>-stress-envelope.json`
captures the 30-rep cold-server envelope so regression-distribution
shifts (rather than single-rep flips) can be detected.

The trigger for a third category: a fixture where neither pattern
fits. Don't lift to a TOML `category` field until that trigger
appears — the two-category split lives in fixture-header comments
for now.

## Doctrine: guards that cannot be auto-recovered

Some guards are categorically not auto-recovery candidates. Encoding
this as doctrine rather than reviewer folklore (the reasoning will
come up repeatedly):

> **Guards that exist as safety brakes against runaway model behavior
> must not have auto-recovery affordances.**

The reasoning: auto-recovery on a safety brake is "ignore the brake."
The brake's purpose is to halt a bad pattern; replacing the halt with
a recovery action assumes the pattern is bad in only one specific way
the recovery handles, which defeats the brake.

`turn_cap` is the clearest case — it fires at `MAX_TURNS=8` and exists
specifically to stop runaway loops. An auto-recovery there would
arbitrarily extend the loop budget under whatever heuristic the
recovery encodes, with no principled stopping point.

`dedup` is the second case — it fires on consecutive-identical-call
loops, and the only "recovery" that mechanically resolves the
underlying issue is exiting the loop, which is what dedup already
does.

Contrast with `read_before_write`: not a safety brake against runaway
behavior, but an *affordance-withholding refusal* — the model can do
the action *once a precondition is met*. The harness can satisfy the
precondition; the auto-recovery doesn't override the refusal, it
discharges it.

This split (safety-brake vs affordance-withholding) is the lens for
the per-guard audit in Tier 2 of the post-Phase-B plan.

## Guard audit rubric

For each guard, four questions in this order. Stop at the first "no";
the guard is not an auto-recovery candidate.

| Guard | Recoverable? | Deterministic? | Local-safe? | Systemic-safe? |
|---|---|---|---|---|
| `read_before_write` | yes (synthesize the precondition) | yes | yes (read is bounded, idempotent) | yes (discharges precondition, doesn't override refusal semantics) |
| `cold_read` | conversational (refusal already steers toward "answer directly") | n/a | n/a | n/a |
| `length` | maybe | maybe | uncertain | uncertain |
| `dedup` | unlikely (loop *is* the failure) | low | likely no | **no** (override of safety brake) |
| `write_pressure` | maybe | uncertain | uncertain | uncertain |
| `turn_cap` | **structurally no** | n/a | n/a | **no** (override of safety brake; see doctrine above) |

The two "safe?" columns are deliberately split. **Local safety** asks
whether the recovery is bounded and correct for the specific case;
**systemic safety** asks whether the pattern, if generalized, weakens
the harness's overall refusal semantics.

A recovery may appear locally safe (the read returns clean bytes) but
be systemically dangerous (teaching the harness to override every
refusal with a synthetic affordance). `read_before_write`'s auto-read
is systemically safe because it satisfies a *precondition* — the
refusal semantics ("don't modify blind") remain intact, just with the
precondition discharged by the harness instead of the model. Any
future recovery should clear both columns explicitly.

## Anti-pattern: precision that encodes model determinism

A predicate set that *looks* precise can silently encode the model's
current deterministic chain as the invariant. When the chain shifts
(model rev, prompt-cache state, llama-server upgrade), the fixture
breaks for reasons unrelated to its purpose.

**Example.** The Phase B work on fixture 12 considered — and
rejected — a "tight" predicate set:

```toml
# REJECTED — encodes model determinism, not harness invariant.
min_tool_calls = 2
max_tool_calls = 2                              # ← brittle
must_call_all_of = ["read_file", "edit_file"]
stop_reason = "FinalAnswer"                      # ← brittle
max_total_tokens = 4500                          # ← brittle
```

This predicate set is *more observable*: it asserts more about the
expected trace. It would also pass cleanly on a warm-cache 3-rep
baseline — the exact trap that hid b-current's 4/10 true rate behind
a misleading 3/3 result.

Across the actual 30-rep cold-server envelope, this predicate set
would fail on:
- the verify-read shape (`tool_calls = 3 > max=2`),
- the length-truncated shape (`stop_reason = "Length"`, tokens > cap),
- the no-call-FA shape (missing `edit_file`),

— even though the *harness invariant* (auto-read fires, model
composes ≥1 tool call beyond it) is satisfied in 87% of those reps.
The "precise" predicates would call those reps regressions; they
aren't.

**The fixture-12 predicates that actually shipped** instead pin the
invariant at the right level of abstraction:

```toml
# ACTUAL — pins invariant, tolerates envelope.
min_tool_calls = 2
max_tool_calls = 3                              # admits verify shape
min_model_tool_calls = 1                        # rejects no-recovery shapes
must_call_any_of = ["edit_file"]
must_have_synthetic_calls = ["read_file"]
must_not_have_synthetic_calls = ["edit_file", "write_file"]
must_fire_guards = ["read_before_write"]
# stop_reason intentionally omitted
max_total_tokens = 7000                         # admits verify p95
max_wall_ms = 75000                             # admits length-trunc envelope
```

Both predicate sets are equally *expressible* in the TOML format.
The first looks tighter and is wrong. The second deliberately omits
axes that would over-constrain. The discipline: **before adding any
predicate, name the invariant it's defending — if the invariant
doesn't require that predicate, leave it out.**

## Worked example: fixture 12

The full predicate set under each of the four axes:

| Axis | Predicate(s) | Defends what invariant |
|---|---|---|
| Kind | `must_call_any_of = ["edit_file"]` | The model composed an edit attempt at some point. |
| Kind | `must_not_call = [..., "bash"]` | Real bash-tool dispatch is a different failure class than the prose-bash markdown loops; don't admit it. |
| Kind | `must_fire_guards = ["read_before_write"]` | The guard intended for this fixture actually fired (vs. the model preemptively reading and bypassing it). |
| Kind | `must_not_fire_guards = [cold_read, dedup, write_pressure, turn_cap]` | Canary set — accidental fires indicate a different bug than this fixture's failure family. |
| Count | `min_tool_calls = 2, max_tool_calls = 3` | Synthetic read + ≥1 model call, ≤2 model calls. Rejects no-recovery and excessive-verify shapes. |
| Count | `min_guard_fires = 1, max_guard_fires = 2` | One read_before_write fire; max=2 admits the length-truncated shape's read_before_write + length co-fire. |
| Count | `min_tool_errors = 0, max_tool_errors = 0` | Guard refusals push ok=true tool_results; auto-read succeeds; model edit succeeds. No errors in any observed shape. |
| Provenance | `must_have_synthetic_calls = ["read_file"]` | The auto-read actually fired (vs. the model emitting its own read_file). |
| Provenance | `must_not_have_synthetic_calls = ["edit_file", "write_file"]` | The harness never synthesizes mutating actions — defensive contract. |
| Compositionality | `min_model_tool_calls = 1` | The model did at least some work beyond observing the synthetic intervention. Load-bearing rejection for no-call-FA and length-truncated shapes. |
| Envelope | `max_total_tokens = 7000` | Bounds the envelope (verify shape p95 ≈ 6355). |
| Envelope | `max_wall_ms = 75000` | Bounds the envelope (length-truncated worst ≈ 69s). |

Three things this set deliberately does **not** assert:

- `stop_reason` — both `FinalAnswer` and `Length` are observable in
  the envelope; `min_model_tool_calls = 1` is what rejects the
  failure variants of each, not the stop reason.
- exact `tool_calls` count — the verify shape's third call is
  task-correct, not a regression.
- exact `total_tokens` value — within the cap, any value is fine; the
  cap is a bound on context damage, not a deterministic-output assertion.

The omitted axes are not oversights. They're deliberate non-assertions
on the principle that predicate precision should defend the invariant,
not maximize observability.

## When to add a new predicate

Two valid triggers:

1. **A new invariant becomes load-bearing.** The compositionality axis
   was added when no-call-FA's existence required distinguishing
   "harness intervention fired" from "model composed something."
   Before that, provenance was sufficient.

2. **An existing fixture's invariant is silently passing trivially.**
   If a fixture passes on a shape that shouldn't satisfy its invariant
   — even though current model behavior never produces that shape —
   the invariant needs a more precise predicate. (This was Reviewer 3's
   catch on fixture 12 Phase B before it shipped.)

Invalid triggers (don't add a predicate for these):

- *The fixture seems light* — predicate count is not a quality metric.
- *Lock down current observed behavior* — that's the "encode model
  determinism" anti-pattern.
- *Future maintainers might be confused* — write a comment in the
  fixture header instead. Comments degrade gracefully; over-tight
  predicates flake CI.

## When to remove a predicate

A predicate should be removed when:

- It's an artifact of an old shape that no longer represents the
  invariant (the post-(b) fixture-12 changes removed `stop_reason`
  for exactly this reason).
- It's defended by another predicate at a more appropriate axis
  (e.g., `must_not_call = ["read_file"]` could have been used on
  fixture 12 to assert "model doesn't redundantly read," but
  `max_tool_calls = 3` defends the same invariant at the count axis
  and doesn't fight provenance).

Removal is a code change; document the reason in the commit message.

## Related artifacts

- `bench/STRESS-PROTOCOL.md` — the reps-10 cold-server discipline for
  characterizing a fixture's stochastic envelope before predicates
  are locked. Required reading before adding a new guard-intervention
  characterization fixture.
- `bench/baselines/main/<fixture-id>-stress-envelope.json` — persisted
  envelope artifacts for guard-intervention fixtures. Compare against
  fresh stress runs to detect distribution shifts.
- `lessons.md` 2026-05-17 (third and fourth entries) — the empirical
  arc that produced this framework.
