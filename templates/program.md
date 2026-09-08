# Optimizing this repository with autor3search-rust

You are an autonomous coding agent working in this repository with one job:
propose small, targeted performance optimizations and let a separate,
compiled measurement harness — `autor3search-rust` — decide whether each one
is kept or thrown away. You cannot influence the measurement itself; you can
only propose a diff and read the verdict it produces.

A human has already run `autor3search-rust init` (which wrote this file and
`.autor3search/config.yaml`) and `autor3search-rust baseline --tag <tag>`
(which created the run branch, pinned the baseline commit, and froze the test
files). You do not need to run either of those yourself.

## The loop

1. Read `.autor3search/config.yaml` to see what is measured (`benchmarks`),
   what you may edit (`scope`), and how strict the gates are.
2. Make **one** focused change within `scope` — ideally a single, mechanical
   optimization you can describe in a sentence.
3. Run `autor3search-rust eval --desc "<what you changed and why>"`.
4. The harness builds the repository, runs its test suite, measures your
   change against the frozen baseline, and returns a verdict as the process's
   exit code (table below).
5. **KEEP** (exit 0): commit your change yourself — `eval` measures and
   scores, it never commits. Then move on to the next idea.
6. **DISCARD** (exit 1): revert your change (`git checkout -- <paths>` or
   `git reset --hard`, whichever matches what you touched) before trying
   anything else. Do not stack a new idea on top of a discarded one.
7. **FAIL** (exit 2) or **CRASH** (exit 3): read the message. It names the
   exact problem — a locked file, a failing test, a tampered frozen test, a
   build error — rather than a generic failure. Fix it or revert; do not
   re-run hoping it goes away on its own.
8. Repeat until you are out of ideas, or until `autor3search-rust status`
   reports `stop_requested: true` (see below), whichever comes first.

## Commands you will use

Every command accepts `-C <dir>` (default `.`) to run against a directory
other than the current one, without `cd`.

- `autor3search-rust eval --desc "<description>"` — run one experiment:
  build, test, measure, verdict. The exit code *is* the verdict. Add
  `--json` to get one JSON object on stdout instead of human-readable text —
  prefer parsing that over the text form.
- `autor3search-rust status [--json]` — reports where the run is: branch,
  pinned baseline worktree, how many experiments have been kept or
  discarded, and whether a human has requested a stop. Never writes anything
  — checking on a run cannot change it.
- `autor3search-rust profile` — profiles the declared benchmarks and reports
  the hottest symbols. Run this before guessing; it turns "try something"
  into "look here." If no profiler is installed on this machine it prints
  how to install one and exits 0 — it never fails a run.
- `autor3search-rust report` — summarizes `results.tsv`: the cumulative
  speedup so far (the product of every kept experiment's own score) and one
  row per experiment, kept or discarded.
- `autor3search-rust doctor` — checks whether this machine can measure
  reliably right now: thermal throttling, CPU frequency scaling, load
  average, disk space, running on battery. Always exits 0; read its output,
  especially before a long unattended run.
- `autor3search-rust version` — which build of the harness produced the
  numbers in `results.tsv`. A result is only as reproducible as the binary
  that made it.

`stop` is a human-issued command, not one you call yourself. You observe it
by checking `status`'s `stop_requested` field between experiments and ending
the session cleanly — finish or abandon whatever `eval` is in flight
according to what the human asked for, and do not start a new one.

## Exit codes from `eval`

| Exit | Verdict | Meaning |
|---|---|---|
| 0 | KEEP | A significant improvement with no disallowed regression. Commit it. |
| 1 | DISCARD | No significant improvement, or a regression that tripped the guard. Revert it. |
| 2 | FAIL | A correctness or scope problem — a locked file was touched, a test failed, a frozen test was tampered with, the config was edited. Not a harness bug: the message names exactly what happened. |
| 3 | CRASH | The repository did not build, or the harness itself hit an error running the pipeline. |
| 64 | (usage) | Bad flags. Fix the command line. |

## Reading `--json`

`--json` prints exactly one JSON object and nothing else on stdout. The
fields you will branch on: `status` (`keep` / `discard` / `fail` / `crash`),
`reason` (a short machine-readable code such as `no_significant_improvement`,
`guard_regression`, `scope_violation`, `inline_test_modified`,
`build_failed`, or `tests_failed`), `score` (the geometric mean of
candidate/baseline time ratios across the declared benchmark set — below 1.0
is faster, and this is what `min_effect_pct` and `max_regress_pct` in the
config are measured against), `message` (the human-readable explanation),
`regressions` (any benchmark that got significantly slower than the guard
allows), `warnings` (measurement caveats, such as too few rounds for a
bounded confidence interval), `stop_requested` (a human has asked this run to
end), and `run` (the tag identifying this run).

## What you may edit

Only paths matched by `scope` in `.autor3search/config.yaml` — by default
`src/**`. You may edit any Rust source file matched by scope: add, remove,
move, or restructure code, including moving code between files, as long as
the result stays within scope and still compiles. Editing a path outside
`scope` is not silently ignored — it fails the experiment with
`scope_violation`, naming the file.

## MUST NOT

- Edit `.autor3search/config.yaml`. It is hashed at baseline; any change
  fails the experiment with reason `config_changed`.
- Edit anything under `tests/**` or `benches/**`. These are snapshotted at
  baseline and restored byte for byte before every `eval` — any edit you make
  there is silently erased before measurement runs, so it can never help you
  and only spends a turn you could have used on something that does.
- Edit a `#[cfg(test)] mod tests` block or a doctest inside a source file you
  are otherwise allowed to change. These cannot be restored for you —
  restoring the file would erase your optimization too — so they are hashed
  at baseline and any change fails the experiment with reason
  `inline_test_modified`. Revert the test and re-run.
- Edit `Cargo.toml`, `Cargo.lock`, `.cargo/config.toml` or
  `rust-toolchain.toml`. These are rejected outright regardless of scope.
  `.cargo/config.toml` in particular can set `RUSTFLAGS`: adding
  `-C target-cpu=native` produces a real, reproducible speedup with no logic
  change at all, which is not what this run is measuring.
- Weaken, delete, or skip a test to make it pass. A failing test means your
  change is wrong; fix the change, not the test.
- Change what a benchmark computes in order to change how fast it appears to
  compute it. A benchmark that got faster because it now does less work is
  not an optimization — it is a discarded result waiting to happen, and the
  test suite outside your scope exists to catch exactly this.
- Call `eval` again on the same uncommitted change hoping for a better roll.
  Measurement noise is exactly why rounds are interleaved and compared with a
  significance test — it is not a reason to re-roll an unchanged `DISCARD`.
- Reach for `unsafe` as a shortcut to a faster number. See the note at the
  end of the idea bank below: it is not gated in this version, and that is a
  gap in the harness, not permission from it.

## MUST

- Make exactly one change per `eval`. A batch of unrelated edits that gets
  `KEEP` tells you nothing about which part of it worked; one that gets
  `DISCARD` tells you nothing about which part of it hurt.
- Write a `--desc` that states what you changed and why you expected it to be
  faster, in one line. It is the only record a human reading `results.tsv`
  later will have for that row.
- Revert a `DISCARD` completely before trying the next idea, rather than
  editing further on top of it.
- Read `profile`'s output before guessing. It is real evidence about where
  time actually goes in this repository, not a generic checklist.
- Stop and report back once `status` shows `stop_requested: true` and your
  current experiment, if any, has finished — do not start another one.

## Rust optimization idea bank

Search rather than flail. Start from `autor3search-rust profile`, not from a guess.

- **Allocation in a hot loop.** `String`/`Vec` growth without `with_capacity`;
  building a `String` a character at a time; `format!` where `write!` into a
  reused buffer would do.
- **`clone()` on a hot path.** Often a borrow, a `Cow<'_, str>`, or moving
  instead of copying.
- **`collect()` into an intermediate collection** that is immediately consumed.
  Iterator chains usually fuse without it.
- **`&str` vs `String` parameters.** Taking `String` forces a caller to
  allocate; taking `&str` does not.
- **Bounds checks.** Indexing in a loop where iterators, `chunks_exact`, or a
  single up-front slice would let the compiler drop them.
- **Dynamic dispatch.** `Box<dyn Trait>` in a hot call; a generic parameter
  monomorphizes and inlines.
- **Hasher choice.** `HashMap`'s default is SipHash, chosen for DoS resistance;
  a faster hasher is appropriate for internal, non-adversarial keys.
- **`#[inline]` on small cross-crate functions**, which are not inlined across
  a crate boundary without it or LTO.
- **Autovectorization.** `chunks_exact` plus a fixed-size inner loop often
  vectorizes where a general loop does not.
- **Algorithmic replacement.** The largest wins are usually not micro-tuning:
  a quadratic string build, a linear scan that could be a lookup, work repeated
  per iteration that could be hoisted.

Two notes specific to this harness. There are **no allocation counts** —
criterion measures time only — so an allocation hypothesis has to be confirmed
by the clock, not by a counter. And **`unsafe` is not gated**: `get_unchecked`
will genuinely be faster and will genuinely pass the tests, and it buys that
speed with undefined behaviour on any input the tests do not cover. Do not
reach for it.

## What this harness cannot see, so don't rely on it

- **No allocation counts anywhere in its output.** Not in `eval`'s result, not
  in `profile`. Criterion measures wall-clock time only; an allocation
  hypothesis is confirmed by whether the clock moved, never by a counter this
  tool does not have.
- **No lint gate.** Clippy is not run as a gate here — it is far too
  opinionated to force on an arbitrary repository. Passing or failing clippy
  has no bearing on any verdict.
- **No data-race detector, and that is not a gap.** Safe Rust cannot have a
  data race in the first place; there is nothing here for a `-race`-style tool
  to catch that the compiler didn't already refuse to compile.
