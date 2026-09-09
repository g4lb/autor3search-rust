# autor3search-rust

**Autonomous AI-driven performance optimization for any Rust repository.**

Point your coding agent at your repo and go to sleep. It proposes an
optimization, runs it through a frozen measurement harness, and the harness
decides: **KEEP** or **DISCARD**. You wake up to a log of experiments and
faster code.

A port of [autor3search-go](https://github.com/g4lb/autor3search-go), which
does this for Go — where the metric is `ns/op` instead of criterion's
`estimates.json`, and where **correctness is not optional**. Inspired in turn
by [karpathy/autoresearch](https://github.com/karpathy/autoresearch), which
does this for a single-GPU LLM training loop.

> **Status: early.** This release is proven by its own test suite — 240+
> tests, including end-to-end tests that drive `eval` to a real `KEEP` and a
> real `DISCARD` against the bundled demo crate — and by one real measured
> run against that crate, recorded in full in
> [the case study](docs/case-study.md): a quadratic `String` rebuild replaced
> with `String::with_capacity` + `push`, measured at **−13.57 %**
> (44,702 ns → 38,636 ns, `p ≈ 1.1 × 10⁻⁵`), `KEEP`. It has **not** yet been
> run against a third-party crate from crates.io with an agent driving
> `program.md` end to end — see
> [Validation status](#validation-status-stated-exactly) before you point it
> at something you care about. Every number in this README and in the case
> study is a real measurement, never an illustration.
>
> What version you get, and what changed in it, is on the
> [releases page](https://github.com/g4lb/autor3search-rust/releases) — this
> README describes the current one.

---

## Start here

Open your coding agent inside the Rust repository you want to make faster,
and paste this:

```text
Install and run autor3search-rust on this repository, then optimize it.

Setup:
1. cargo install --locked autor3search-rust
   Make sure ~/.cargo/bin is on PATH.
2. autor3search-rust init
   Show me the benchmarks it discovered. If it reports none, STOP and tell
   me: this tool can only optimize what it can measure.
3. git add -A && git commit -m "autor3search-rust init"
4. autor3search-rust doctor
   Show me any warnings. If the machine looks unfit to measure, stop and ask
   me before continuing.
5. autor3search-rust baseline --tag <today, e.g. sep6>

Then:
6. Read program.md in this repository, in full. It is your instruction set
   for the rest of this run. Follow it exactly.

Rules for the whole run:
- Never edit program.md, .autor3search/config.yaml, results.tsv, or anything
  the harness writes. They are not yours.
- Never pass --force to any autor3search-rust command. (I may run
  `autor3search-rust stop --force` myself; that one is mine, not yours.)
- One idea per experiment. Commit before each eval.
- KEEP means the commit stays. Anything else means git reset --hard HEAD~1.
- Print one context line before each experiment, so I can see where you are:
  [exp <n> | <branch> | vs <measure_commit> | stop: autor3search-rust stop]

Run the loop until I stop you. I stop you by running `autor3search-rust stop`
in my own terminal — you will see it as "stop_requested": true in a verdict.
When you do: apply that verdict, do not start another experiment, run
`autor3search-rust report`, summarize what you tried, and exit the loop.
```

That's the whole handoff. The agent installs the tool, sets the run up, and
then follows `program.md` — which the harness generated for your repository
and which tells it how to run the keep-or-discard loop.

What you get back: one commit per accepted change on a branch named
`autor3search-rust/<tag>`, and a `results.tsv` recording every experiment
that was tried, including the ones that failed. `autor3search-rust report`
summarizes it.

Two things worth knowing before you start it:

- **It needs benchmarks.** The tool optimizes what it can measure, and
  refuses to guess. See
  [Repos with no benchmarks](#repos-with-no-benchmarks).
- **Numbers are only as good as the machine.** Run `doctor` and read it. A
  thermally throttled laptop on battery produces noise dressed as data.

## The idea

You do not edit Rust source files to tune performance. You edit
`program.md` — the instructions that drive your agent. The agent edits the
Rust. A compiled harness holds the metric, and the agent cannot reach it.

| Piece | What it is | Who edits it |
|---|---|---|
| `autor3search-rust` | the harness binary: gates, measures, scores | nobody — it's compiled |
| your `tests/**` and `benches/**` files | frozen at baseline, restored before every run | nobody — restored automatically |
| your `src/**` | whatever is in `scope` | **the agent** |
| `program.md` | the agent's instructions | **you** |
| frozen tests, baseline worktree, baseline record | lives outside your repo, under `dirs::cache_dir()` (or `AUTOR3SEARCH_RUST_STATE_HOME`) | nobody — the agent could not reach it even by editing every file in scope |

That last row matters: the agent edits the repository, so anything the score
depends on that *lived* there would be silently writable by the very agent
it is meant to constrain — including the pinned baseline worktree, where
making the *baseline* slower is a cheaper win than making the candidate
faster. The only harness output that stays inside your repo is
`results.tsv` (a human-readable log, not part of the metric) and `run.log`
(subprocess transcripts) — both gitignored by `init`.

There is a fourth kind of test this table doesn't capture: a
`#[cfg(test)] mod tests` block or a doctest **inside** a `src/**` file you
are otherwise allowed to edit. Those cannot be frozen as a whole file
without also erasing whatever real optimization sits next to them — see
[What the harness enforces](#what-the-harness-enforces).

## Quick start

```bash
cargo install --locked autor3search-rust

cd your-rust-project
autor3search-rust init                                # find benchmarks, write config + program.md
git add -A && git commit -m "autor3search-rust init"  # baseline refuses a dirty tree
autor3search-rust doctor                              # is this machine fit to measure?
autor3search-rust baseline --tag sep4                 # freeze tests, pin the baseline commit
```

That commit matters — `init` only writes files, it does not commit them, and
`baseline` refuses to run against an uncommitted tree because a baseline
pinned against what's on disk (not what's in git) would not be reproducible.
`init` writes three things: `.gitignore` entries, `.autor3search/config.yaml`,
and `program.md`. `.autor3search/config.yaml` is the one file under
`.autor3search/` that gets committed — it's the run configuration, and
humans own it; everything else the harness later writes under
`.autor3search/` is gitignored.

Then start your agent in the repo:

```
Read program.md and start the optimization loop.
```

It runs until you stop it. Each experiment is one commit, one verdict, one
row in `results.tsv`.

## Watching a run, and stopping it

The agent's loop calls `eval --json`, which by contract prints one JSON
object and nothing else — so there is no human-readable stream to watch.
Ask the run where it is instead, from any terminal, any branch, at any time:

```
$ autor3search-rust status
run tag        proof
branch         autor3search-rust/proof  (checked out)
baseline       19a7b01  (run started here)
measuring vs   55553fb  (advanced past the baseline by earlier KEEPs)
worktree       /Users/you/Library/Caches/autor3search-rust/4799b987556c0f52/proof/baseline-worktree
experiments    1 run  (1 keep, 0 discard, 0 fail, 0 crash)  — next is #2
stop           not requested

to stop after the current experiment:  autor3search-rust stop
to stop now, abandoning it:            autor3search-rust stop --force
```

(That is real output, from the run recorded in the
[case study](docs/case-study.md).) `status` never writes anything: checking
on a run cannot change it.

There are three ways to stop, and they differ in what happens to the
experiment currently in flight.

**`autor3search-rust stop` — graceful.** Writes a request the agent reads at
its next verdict. The experiment under way finishes and is scored, its KEEP
or DISCARD is applied, and only then does the loop exit with a summary.
Nothing is thrown away. This is the one to use.
`autor3search-rust stop --clear` cancels it if you change your mind before
the agent notices.

**`autor3search-rust stop --force` — immediate.** For when you cannot wait
out a long benchmark. It writes the same request, then signals the running
`eval` to abandon the experiment. The experiment is lost (no `results.tsv`
row is written, because nothing was measured); every kept commit before it
is untouched. It then tells you what state the repository is in, including
the commit the agent had made for the abandoned experiment and how to drop
it. It does not drop anything for you.

On unix, `--force` sends a real `SIGTERM` to a verified-live `eval` process
and waits up to 10 seconds for it to release its claim on its own — `eval`'s
signal handler sets a cancellation flag that `runner`'s subprocess wait loop
and the pipeline both check every 20 ms, between measurement rounds and
between gates, so the process gets a real chance to record
`ABORTED`/`stop_forced` and tear down its own benchmark subprocess (a
grandchild, in its own process group) cleanly rather than leaving it
running. **This has a real gap, stated plainly:** there is no
grace-then-SIGKILL escalation. If `eval` never reaches a poll tick — a
genuinely wedged process, not the designed path — `--force` cannot clean it
up, and you would need to kill it by hand. This is a smaller gap than it
looks: an external `SIGKILL` aimed at `eval`'s own pid would not reach a
wedged benchmark grandchild in a separate process group either, so
escalating the signal buys less than it appears to. **The Windows path
(`OpenProcess`/`TerminateProcess`) is implemented and exercised by CI, but
has not been verified on real Windows hardware** as part of this release —
see [Validation status](#validation-status-stated-exactly).

**Ctrl+C.** Interrupting the agent works too. `eval` handles the signal
rather than dying under it, which matters more than it sounds: criterion
runs the compiled benchmark as a grandchild process, so an `eval` killed
without a chance to clean up would leave that benchmark running — burning
CPU and corrupting every later measurement on the machine.

Whichever you use, the work is on the run branch `autor3search-rust/<tag>`
and `autor3search-rust report` summarizes it. Resuming later needs nothing
special: clear any pending stop and point the agent back at `program.md`.

## Commands

| Command | What it does |
|---|---|
| `init` | Scans the repo with `cargo metadata` and criterion `--list`, and writes `.autor3search/config.yaml` + `program.md`. Refuses to overwrite an existing config without `--force`. |
| `doctor` | Checks whether this machine can measure reliably (CPU frequency scaling, thermal throttling risk, load average, disk space, on-battery, `CARGO_INCREMENTAL`, a `[profile.bench]` override, a nightly default toolchain) and prints its findings. Informational — always exits 0. |
| `baseline --tag <tag>` | Creates the run branch `autor3search-rust/<tag>`, freezes every in-scope `tests/**`/`benches/**` file and hashes every in-scope `src/**` file's inline tests and doctests, and pins a detached worktree at the baseline commit. Refuses a dirty tree and a reused tag. |
| `profile` | Runs the declared benchmarks under criterion's `--profile-time` with [samply](https://github.com/mstange/samply) attached, and prints the top self-time symbols — real sampling-profiler data on where time actually goes, rather than an agent guessing from reading source. When samply is not installed, prints how to install it (`cargo install samply`) and exits 0 rather than failing the run. |
| `eval` | Runs one experiment: gates (scope, locked files, config integrity, restore, inline-test check, frozen-set check, build, test, worktree integrity), measures the candidate against the pinned baseline worktree, scores it, appends a `results.tsv` row, exits `0`/`1`/`2`/`3` for KEEP/DISCARD/FAIL/CRASH, and on `KEEP` re-points the pinned worktree at the candidate's commit so the next `eval` measures against it (see [Scoring](#scoring)). |
| `status` | Prints where a run is: run branch and whether it is checked out, the frozen baseline commit and the advancing measurement commit, the pinned worktree, how many experiments have run and with what verdicts, and whether a stop is pending. Read-only. Accepts `--tag <tag>` so it works from any branch. |
| `stop` | Asks the agent to end the run after the experiment it is running: writes a request `eval` reports back as `stop_requested`. `--clear` cancels a pending request; `--force` additionally signals the running `eval` to abandon the current experiment and reports what state that leaves the repository in. Accepts `--tag <tag>`. |
| `report` | Summarizes `results.tsv`: counts by status, the cumulative speedup as the product of every kept experiment's score, and the largest individual wins. |
| `version` | Prints which build of the harness is running: the crate version for an installed binary, or the commit for one built from a checkout (marked `dirty` when the tree had uncommitted changes). A `results.tsv` row is only as reproducible as the binary that produced it. |

Every command accepts `-C <dir>` to run against a repository other than the
current directory, rather than changing the process's working directory —
safer under concurrent invocations, and testable without changing the
process's own working directory.

Flags are double-dash throughout (`--tag`, `--force`, `--desc`, `--json`,
`--clear`, `--no-log`), unlike the Go original's single-dash long flags
(`-tag`, `-force`). This is deliberate, not an oversight: `program.md` and
this README are generated for this tool and spell its own flags throughout,
so nothing an agent copies out of them is wrong. A single-dash long flag
(`-force`) is refused outright, naming the two-dash spelling in the error,
rather than silently accepted as an alias — accepting both would let a
copy-pasted Go instruction appear to work in a repository where it measures
the wrong thing.

### Where run state lives

Everything the metric depends on — the frozen golden test copies, the
baseline record, the pinned worktree — is kept **out of the repository**,
under `<user cache dir>/autor3search-rust/<repo hash>/<tag>/`. That is
deliberate: the agent edits the repository, so in-tree state would be state
the agent could rewrite to make itself look good.

Set `AUTOR3SEARCH_RUST_STATE_HOME` to an absolute path to put it somewhere
else — useful in a container or CI runner with no durable cache. Run state
is keyed underneath it the same way, one directory per repository and one
per tag. A relative value is refused: it would resolve against whatever
directory each command happened to run from, so `eval` from a subdirectory
and `stop` from the repository root would address different state for the
same run.

## Worked example

A word counter. Ordinary Rust, with ordinary tests. This is the exact crate
and the exact run recorded in [the case study](docs/case-study.md) — every
number below came out of it.

**`src/lib.rs`** — the code the agent is allowed to change:

```rust
//! A word counter, written the slow way on purpose.

use std::collections::HashMap;

/// Returns how many times each lowercase word appears in `s`.
///
/// ```
/// let counts = demo::count_words("the quick brown the");
/// assert_eq!(counts["the"], 2);
/// assert_eq!(counts["quick"], 1);
/// ```
pub fn count_words(s: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for field in s.split_whitespace() {
        let mut word = String::new();
        for c in field.chars() {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                // Quadratic on purpose: rebuilds the string every character.
                word = word + &c.to_string();
            }
        }
        if !word.is_empty() {
            *counts.entry(word).or_insert(0) += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_repeated_words() {
        let got = count_words("the quick brown the");
        assert_eq!(got["the"], 2);
        assert_eq!(got["quick"], 1);
        assert_eq!(got["brown"], 1);
        assert_eq!(got.len(), 3);
    }
}
```

**`tests/wordcount.rs`** and **`benches/wordcount.rs`** — the integration
tests and the criterion benchmark. **The agent cannot change these files.**
They are hashed at `baseline` and restored before every single evaluation.
The `#[cfg(test)] mod tests` block above and the `///` doctest inside
`src/lib.rs` **cannot** be restored the same way — restoring the whole file
would erase the agent's optimization too — so they are hashed instead, and
any change to either is a hard `FAIL(inline_test_modified)`, not a silent
erasure.

**The agent's change** — preallocate the `String` and push into it instead
of rebuilding it on every character:

```rust
let mut word = String::with_capacity(field.len());
for c in field.chars() {
    let c = c.to_ascii_lowercase();
    if c.is_ascii_lowercase() || c.is_ascii_digit() {
        word.push(c);
    }
}
```

**The result.** Both versions run the identical frozen test files, and both
pass them. Measured with 10 interleaved rounds per side (plus one discarded
warmup round):

| metric | before | after | change | p |
|---|---:|---:|---:|---:|
| `count_words` median time | 44,702 ns | 38,636 ns | **−13.57 %** | ≈1.1 × 10⁻⁵ |

Every one of the 10 candidate-round medians measured faster than every one
of the 10 baseline-round medians — complete separation, which is why `p` is
so far below both the raw `alpha = 0.05` and the corrected bar. Score is
`0.8643`. Below `1 - min_effect_pct/100` (0.99 at the default) and
significant, so the harness returns:

```
VERDICT: KEEP (improved) score=0.8643
score 0.8643 (-13.57%)
```

The commit stays, the branch advances, and the measurement baseline
advances with it — the *next* experiment is measured against this commit,
not against the original one. Had the change been slower, broken a test, or
been indistinguishable from noise, the harness would have returned
`DISCARD` or `FAIL` and the agent would `git reset --hard`. This run's own
`report` afterward:

```
autor3search-rust report: 1 experiment(s)  (1 keep, 0 discard, 0 fail, 0 crash)

cumulative speedup: 13.6%  (score 0.86, 1.16x faster overall)

largest wins:
  1. with_capacity + push (time: -13.6%)
```

There are no `B/op`/`allocs/op` rows in this table the way the Go worked
example has them — criterion measures wall-clock time only. See
[Limitations](#limitations) for what that costs.

## What the harness enforces

An agent optimizing your code can "win" by cheating. Each route is closed:

| Cheat | Why it fails |
|---|---|
| Weaken or delete a `tests/**`/`benches/**` file | hashed at baseline and **restored** before every run — edits are erased, not argued about |
| Weaken an inline `#[cfg(test)]` assertion | hashed at baseline over its parsed token text; any change is a hard `FAIL(inline_test_modified)` — refused, not restored, since restoring the whole file would erase the agent's real edit too |
| Weaken a doctest | same: doctest text is hashed at baseline, and any change is `FAIL(inline_test_modified)` |
| Delete the work a benchmark measures | the frozen tests still run and still assert the real behavior |
| Add an easier benchmark | any `tests/**`/`benches/**` file absent from the frozen manifest is rejected |
| Replace a frozen test file with a symlink to a file outside the repo | freezing refuses to snapshot a symlinked test file, and restoring refuses to write through one that appears later — both fail loudly instead of writing through the link |
| Edit files outside the agreed area | `scope` violations fail before anything is even built |
| Set `-C target-cpu=native` in `.cargo/config.toml` (or the extensionless `.cargo/config`) | locked outright, regardless of scope — a real, reproducible speedup with no logic change at all, which is not what a run is measuring |
| Swap or edit a dependency, or the toolchain (`Cargo.toml`/`Cargo.lock`/`rust-toolchain.toml`/`rust-toolchain`) | rejected outright regardless of `scope` — a dependency or toolchain change is a human decision, not an autonomous one, and would change what is being measured rather than how fast it runs |
| Bank measurement noise as a win | a Mann-Whitney test must clear `p < 0.05`; noise is `DISCARD` |
| Speed up A by wrecking B | any significant regression over 5 % rejects the change outright |
| Loosen the rules mid-run (raise `max_regress_pct`, narrow `scope`, drop a benchmark) | `.autor3search/config.yaml` is hashed at baseline; any change to it fails the run with a config-hash mismatch |
| Compare against a stale baseline | the measurement baseline is **re-measured every run**, interleaved with the candidate |
| Coast to `KEEP` on an earlier improvement doing nothing new | the measurement baseline **advances to the newly kept commit after every `KEEP`** (see [Scoring](#scoring)), so a later no-op is compared against what was just kept, not against where the run started |

That last one matters more than it looks. Comparing a candidate measured now
against a baseline measured an hour ago on a cooler CPU attributes thermal
drift to your code change. Alternating both sides in one session cancels it.

One cheat is deliberately **not** in this table. See
[Limitations](#limitations) — `unsafe` is not gated in this release, and a
reader deserves to know that before pointing an unattended loop at their code.

## Scoring

One number, so nothing can be cherry-picked:

```
score = geomean(new_ns / base_ns)   across the declared benchmark set
```

`KEEP` requires **all** of:

1. **A minimum real improvement.** `score` must be below
   `1 - min_effect_pct/100` (default `min_effect_pct: 1.0`, i.e.
   score < 0.99), not merely below 1. A change that is technically
   significant but trivially small is not worth a commit in an unattended
   overnight loop.
2. **A Bonferroni-corrected significant improvement.** At least one
   benchmark's p-value must clear `alpha / k`, where `k` is the number of
   benchmarks compared in that experiment — not the raw `alpha` (0.05).
   Testing `k` benchmarks against the same uncorrected `alpha` inflates the
   chance that at least one shows a spurious "significant" result purely by
   chance (with 4 benchmarks, about an 18% chance); dividing `alpha` by `k`
   is the standard correction for that.
3. **No significant regression beyond `max_regress_pct`** (default 5%).
   This guard deliberately uses the raw, **uncorrected** `alpha`, not the
   Bonferroni-corrected one from rule 2 — on purpose, and it looks
   inconsistent until you see why: the correction in rule 2 only ever makes
   it *harder* to call a result significant, and applying it to the
   regression guard would make real regressions *easier* to miss. We want
   the opposite bias for harm: conservative about accepting a win, liberal
   about catching damage.

A `Delta`'s reported significance (`p < alpha`, no correction) is always the
raw, honest statistic — that's what a human or agent should see when
reading a report. The Bonferroni correction in rule 2 is a KEEP-decision
threshold layered on top, not a redefinition of "significant"; `eval`'s
output calls out a benchmark that is significant at `alpha` but did not
clear the corrected bar, rather than silently calling it "not significant."

A discard distinguishes the two ways rule 1 and rule 2 can fail.
`no_significant_improvement` means nothing measurably moved.
`improvement_below_min_effect` means the change really did speed things up,
by less than `min_effect_pct` — worth knowing, because it says the idea was
directionally right rather than inert.

`base_ns` is **not** fixed for the whole run. `baseline` pins two things
that are kept deliberately separate: a FROZEN commit that the frozen tests
and the scope gate always compare against (so an agent cannot expand what
it may edit by banking experiments), and a MEASUREMENT commit — what
`base_ns` is actually measured against — that starts equal to the frozen
one and **advances to the candidate's own commit after every `KEEP`**. So
`score` always answers "did *this* experiment help, compared to the last
thing that was kept," never "is the tree better than when the run started."
Without this, once one real improvement was kept, every later
experiment — however useless — kept comparing against that same stale
starting point and a no-op could coast to `KEEP` on an earlier win it did
not contribute to.

One consequence: each kept `score` is now only that experiment's own
incremental contribution, so `report`'s cumulative speedup is the
**product** of every kept score, not the latest one alone — successive real
improvements compound the way percentage changes do.

## Limitations

Stated plainly, because performance tools that oversell are worse than
useless.

**Ported from the Go original, and just as true here:**

- **A `KEEP` is evidence, not proof.** Any fixed significance threshold
  admits some false positives by construction — that's what "alpha" means.
  Measuring genuine no-op trials (a commit that changes only a comment)
  against the pre-Bonferroni rule produces spurious `KEEP`s at roughly a
  3–5% rate on the Go original, consistent with running several benchmarks
  per experiment against `alpha = 0.05` each. The minimum-effect floor and
  the Bonferroni correction described in [Scoring](#scoring) make the rule
  substantially stricter — the family-wise correction directly targets the
  multiple-comparisons inflation, and the effect floor throws out wins too
  small to matter even when real — but they reduce the false-`KEEP` rate,
  they do not (and cannot) eliminate it. Treat a single `KEEP` as evidence
  worth banking, not as proof the change works. On a machine doing other
  work, a no-op's measured delta can land several percent from zero with a
  convincing p-value. `min_effect_pct` is the knob for this: raise it on a
  noisy machine, since it costs you only wins smaller than the noise you
  cannot measure anyway.
- **Laptops are noisy.** Scheduler and P/E-core effects make numbers jump.
  Interleaving and `count` mitigate it and `doctor` warns you, but a quiet,
  dedicated Linux box gives cleaner results.
- **No benchmarks, no value.** This optimizes what it can measure. `init`
  tells you plainly rather than pretending — see
  [Repos with no benchmarks](#repos-with-no-benchmarks).
- **A small measurement asymmetry remains.** Each round measures the
  baseline a moment before the candidate, so on a machine that is steadily
  warming up, the candidate is consistently sampled a fraction hotter.
  Interleaving cancels the drift *between* rounds, which dominates; this
  sub-second offset does not cancel. It is small next to benchmark
  variance, but it is a known asymmetry rather than an absent one.
- **Microbenchmarks are not your application.** A 50% win on a hot function
  may be invisible end to end. Benchmark what actually matters.
- **`count` below 4 can never reach significance.** The Mann-Whitney test
  behind `p` has a best-case two-sided p-value of 0.1 at 3 measured rounds
  per side — above the 0.05 threshold no matter how large or how clean the
  real improvement is. Every experiment would be discarded on a
  technicality, not on its merits. `config.yaml` refuses `count` below 4
  rather than let that happen silently.

**Rust-specific, and new to this port:**

- **No allocation metrics at all.** Go gets `B/op` and `allocs/op` free from
  `-benchmem`. Criterion measures time only, and counting allocations would
  mean installing a `#[global_allocator]` in the user's benchmark
  harness — editing the very code under measurement. So `results.tsv` drops
  the `allocs_delta` column and carries five fields, not six.

  This costs more than a column. Go's `program.md` idea bank leans on
  allocation counts as the agent's single best lead for *what to try next*;
  the Rust idea bank had to be rebuilt around what remains — `samply`'s hot
  symbols, plus Rust-specific leads: `String`/`Vec` reallocation and
  `with_capacity`, `clone()` in hot paths, `collect()` into an intermediate
  collection, `Box<dyn Trait>` dynamic dispatch vs generics, iterator chains
  vs indexing and the bounds checks each implies, `HashMap` hasher choice,
  `&str` vs `String` parameters, `#[inline]` on small cross-crate functions,
  `chunks_exact` for autovectorization, and `matches!`/slice patterns over
  allocation. See `templates/program.md`'s idea bank for the full list.
- **No `go vet` analogue.** Clippy is the nearest thing and is far too
  opinionated to force on an arbitrary repository — most real crates do not
  pass `-D warnings` — so there is no lint gate. Not offered as a config
  flag either: a gate most repos must disable is a gate that teaches users
  to disable gates.
- **No `-race` analogue, and this one is not really a loss.** Safe Rust
  cannot have data races; the compiler is the gate Go needed a runtime
  detector for. Miri would check `unsafe` code for UB but is far too slow
  for a per-experiment gate and does not support most real programs.
- **Inline tests are refused, not restored**, and doctests likewise (see
  [What the harness enforces](#what-the-harness-enforces)). An agent that
  edits a `#[cfg(test)]` assertion or a doctest gets a `FAIL` and a wasted
  experiment slot rather than a silent erasure.
- **An agent can still buy speed with `unsafe`, and this release does not
  stop it.** `get_unchecked` in place of indexing is the cheapest fake win
  available in Rust: it is genuinely faster, it passes every test that
  exercises only the inputs the test suite happens to cover, and it trades
  that speed for undefined behaviour on any input the tests do not. There is
  **no gate against introducing `unsafe`** in this version. If you point
  this tool at a repository unattended overnight, an agent hunting for a
  faster number can and eventually will reach for `unsafe`, and nothing here
  will stop it or even flag it. `allow_unsafe: false` — failing an
  experiment whose diff introduces `unsafe` blocks not present at
  baseline — is the first v2 candidate, tracked as such rather than shipped
  half-done. Until it exists, review every kept diff for `unsafe` yourself.
- **The baseline worktree compiles from cold on a run's first `eval`.** The
  pinned worktree has no warm `target/` the first time it is measured
  against, so the first experiment of a run pays a full release build on
  the baseline side that later experiments do not.

### Validation status, stated exactly

This tool is proven by its own test suite — 240+ tests, including
end-to-end tests that drive `eval` to a real `KEEP` and a real `DISCARD`
against the bundled `testdata/demo` crate — and by the one real measured run
recorded in [the case study](docs/case-study.md) and in the
[worked example](#worked-example) above. It has **not** been run against a
third-party crate from crates.io, with an agent driving `program.md`
end to end, as part of this release. The Go original's README says
"Validated against three real libraries" because it was; this README does
not make a comparable claim, because it would not be true. State what has
been validated and what has not, plainly: one real `KEEP` on one small,
purpose-built benchmark proves the pipeline works end to end and produces a
real, checkable number. It is not evidence that the tool generalizes to a
large, unfamiliar codebase with many benchmarks running for hours
unattended — that run has not happened yet.

Two narrower gaps, named rather than glossed over:

- **`stop --force` has no grace-then-SIGKILL escalation** — see
  [Watching a run, and stopping it](#watching-a-run-and-stopping-it) above
  for the full explanation and why the residual gap is smaller than it
  looks.
- **The Windows `stop --force` path is implemented but unverified on real
  Windows hardware.** CI builds and runs the full test suite on
  `windows-latest` and exercises this path there, but no one has watched it
  work against a real, long-running benchmark on a physical or
  interactively-used Windows machine as part of this release.

## Repos with no benchmarks

`autor3search-rust init` discovers benchmarks with `cargo metadata` (to find
criterion `[[bench]]` targets, reading `harness` from each target's own
`Cargo.toml` entry) and then criterion's own `--list` (to get the exact
runtime benchmark ids). If it finds none, it refuses to write
`.autor3search/config.yaml` and exits with an error, rather than generating
a config with an empty `benchmarks:` list that would silently optimize
nothing.

That refusal is deliberate: `autor3search-rust` has no other notion of
"faster." The verdict — `KEEP`, `DISCARD`, `FAIL`, `CRASH` — is entirely a
function of the declared benchmarks' timings across a baseline and a
candidate. No benchmarks means no signal to gate on, at which point every
candidate would either be rejected for no reason or accepted for no reason.

To use `autor3search-rust` on a repository like this:

1. Add a criterion bench target to `Cargo.toml`:

   ```toml
   [[bench]]
   name = "my_bench"
   harness = false
   ```

   and write a `c.bench_function("name", ...)` covering the code you
   actually want made faster — a plain criterion benchmark, in
   `benches/my_bench.rs`.
2. Benchmark the right thing. A benchmark that exercises a cold path, a
   trivial helper, or a function nobody calls under load produces numbers
   that are entirely real and entirely useless — confident percentages
   attached to work that was never the bottleneck. Benchmark the function,
   loop, or request path that actually dominates the workload you care
   about, ideally informed by a profile of the real program (`autor3search-rust profile`,
   once benchmarks exist) rather than a guess.
3. Re-run `autor3search-rust init` once the benchmark exists. It will pick
   it up and proceed normally.

## License

MIT © 2026 Gal Be
