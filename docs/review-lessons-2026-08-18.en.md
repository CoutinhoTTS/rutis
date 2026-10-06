# Review Methodology and Domain Lessons

> 2026-08-18. Drawn from zcode's review of the full min-cordis Rust port (design v4→v5, implementation review, and simplification review across five rounds), then checked by Dim. This is methodology and domain knowledge, not an implementation checklist. See [simplify-conclusion](simplify-conclusion-2026-08-18.md) for implementation work.
> Each rule has evidence from this project so the method stays concrete.

## 1. Simplification is not “less”; it is “how many times is the same fact represented?”

The correct definition of redundancy is **two sources of truth for one fact, plus an obligation to keep them synchronized**. That synchronization obligation is where bugs come from.

Use one question to judge: **“What question does this mechanism answer?”** If the same question is answered twice, remove one. If the questions differ, leave the mechanisms alone even if they look identical. Struct count, line count, and number of generic parameters are not criteria.

Evidence from this project:

| True redundancy (same question answered twice) | False duplication (looks similar, solves a different question) |
|---|---|
| Reverse dependency map and `last_deps` (same dependency set) | `status_queue` and watch (ordered full history vs latest value) |
| `resolved` flag and `intents_inflight` (both answer “is prior work done?”) | Two watch channels (one-shot completion identity vs continuous state stream) |
| `once`'s `fired` atomic flag and bus lock (same serialization guarantee) | — |

## 2. Test ownership before deleting: explain what the code used to protect against

For every deletion candidate, state “which race or failure did this prevent originally?” **If you cannot say, you do not yet own the deletion.**

This is a judgment call, but ownership is a hard line. All four implementation guardrails had this shape: the design direction was right, but failures came from invariants that were never written down—`Settle` recognizes only a returned `Failed` error; removing `once` must preserve its original position in the snapshot; removing a yield protocol must close the exit race.

Negative evidence: Dim proposed removing `status_queue` and merging two watch channels. Both were rejected after “looks duplicated” reasoning failed to identify what they protected; test evidence later corrected the design.

## 3. For a serialized queue, the queue itself answers “has my earlier work finished?”

If a serial mailbox executes the work, answer “is everything I submitted earlier finished?” by **enqueueing a sentinel intent and joining it**, rather than inferring completion through out-of-band counters, flags, and double checks.

In any actor/mailbox system, first inspect how many layers of out-of-band reasoning the waiter has accumulated.

Project evidence: `settle` combined five external signals (`resolved` / `intents_inflight` / `notify_inflight_drained` / double-checking watch / yielding in `drain_stale`). One `Intent::Settle(TransitionTask)` barrier could replace all of them.

## 4. Complexity added to fix a bug is a scar, not fat; before judging a scar, check its history

Before deciding that a code shape is redundant, inspect its construction history and git log. A shape added to fix a bug can later be misremembered as needless complexity.

Related rule: **when several independent reviewers repeatedly misread correct code, that is a defect in the code itself.** Readability is close to correctness; rewriting for “obvious at a glance” is a simplification even if line count stays the same.

When fixing a pitfall, record the load-bearing invariant in the docs immediately instead of defending it from memory during a later simplification.

Project evidence:

- zcode proposed removing `PhantomData` from three Adapters as a “weird shape,” forgetting that the project hit E0207 (unconstrained type parameter) on day one and used `PhantomData` to solve it.
- The `JoinSet` code in `serial` (spawn item i in the loop body, immediately `join_next`, then spawn i+1) was misread as concurrent dispatch by Dim and Mira across two review rounds. It was rewritten as inline sequential await. Semantics did not change, but serial execution is now obvious.

## 5. Practical rules for multiple reviewers

**Agreement is strong evidence; disagreement is a judgment call; repeated misreading is information.**

- Do not reopen areas where reviewers agree: four independent reviewers reached almost identical conclusions on blockers.
- Resolve disagreements according to project intent. Disagreement tends to concern aggressive vs conservative choices that are defensible preferences, not right vs wrong; this project's intent is “do not change what is not necessary.”
- Verify disputed claims with **evidence** (API docs, build history), not confidence in memory. Codex said the Adapter blanket implementation did not hold; zcode checked Tokio docs and project history and confirmed it did, including confirming “I was wrong.”

## 6. Domain rule: with a serial driver, the sender must cancel before enqueueing

This is specific to lifecycle frameworks / actors. The driver processes intents serially and never gets to “cancel itself while doing work.” The only time to make `apply` observe cancellation is for **the sender to cancel first, then enqueue the intent**.

Project evidence: `dispose()` / `restart()` call `cancel_current()` before `post(Intent::...)` in [`fiber.rs:652`](../crates/rutis/src/fiber.rs#L652) and [`fiber.rs:678`](../crates/rutis/src/fiber.rs#L678). It took one deadlock to see why: without pre-cancellation, the serial driver keeps running `apply`, which never receives the cancellation signal.
