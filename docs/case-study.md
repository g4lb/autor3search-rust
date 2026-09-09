# Case study: `count_words`, measured

This is the record of one real run of `autor3search-rust` against the bundled
demo crate (`testdata/demo`), plus the bugs the implementation review process
found along the way. Every number below is a measurement, copied out of the
actual run's `results.tsv`, `run.log`, and criterion output — none of it is
illustrative. That is the same standard [the README](../README.md) holds
itself to, for the same reason: a performance tool's own numbers are worth
nothing if they are not held to the standard it enforces on everyone else.

## What was measured

`demo` is a word counter (`src/lib.rs`): `count_words(s: &str) -> HashMap<String, usize>`.
The starting implementation rebuilds a lowercase `String` one character at a
time —

```rust
let mut word = String::new();
for c in field.chars() {
    let c = c.to_ascii_lowercase();
    if c.is_ascii_lowercase() || c.is_ascii_digit() {
        // Quadratic on purpose: rebuilds the string every character.
        word = word + &c.to_string();
    }
}
```

— which reallocates and copies on every character, quadratic in the length
of each word. The tests (`src/lib.rs`'s inline `#[cfg(test)]` module, plus
`tests/wordcount.rs`) exercise case-folding, punctuation stripping, digits,
and empty input. `benches/wordcount.rs` is a single criterion benchmark,
`count_words`, run against 200 repetitions of a fixed sentence.

The run:

```
cargo build --release
cd <scratch>/loop-demo   # a fresh copy of testdata/demo
git init -q -b main && git add -A && git commit -qm init
autor3search-rust init
git add -A && git commit -qm "autor3search-rust init"
autor3search-rust doctor
autor3search-rust baseline --tag proof
# edit src/lib.rs, commit
autor3search-rust eval --desc "with_capacity + push"
autor3search-rust report
```

with `AUTOR3SEARCH_RUST_STATE_HOME` pointed at a scratch directory so nothing
touched the real cache.

`doctor` reported:

```
[ok  ] cpu frequency scaling    not checked on this platform
[ok  ] thermal throttle risk    not checked: could not parse `pmset -g therm`
[ok  ] load average             1-minute load average 2.57 (threshold 5.00 for 10 logical cpus)
[warn] disk space               2.0 GB available — a criterion run writes a sample per round and can fail outright if the disk fills mid-run
[ok  ] power source             on AC power
[ok  ] CARGO_INCREMENTAL        not set
[ok  ] [profile.bench]          opt-level not overridden
[ok  ] default toolchain        rustc 1.98.0 (88d9e12ae 2026-08-18)
```

The disk-space warning was not a false alarm for this machine (see
[Housekeeping](#housekeeping) below) — `doctor` was right to flag it, and the
run was watched closely as a result.

## The change

One idea, one commit: preallocate the `String` and push bytes into it instead
of rebuilding it on every character.

```rust
let mut word = String::with_capacity(field.len());
for c in field.chars() {
    let c = c.to_ascii_lowercase();
    if c.is_ascii_lowercase() || c.is_ascii_digit() {
        word.push(c);
    }
}
```

Both versions run the identical frozen test files and both pass them.

## The result

`count`: 10 measured rounds per side (plus one discarded warmup round, per
`measure::interleave`), interleaved and alternating which side goes first,
`sample_size: 50`. Medians below are `median.point_estimate` out of
criterion's own `estimates.json`, taken from the actual `round-1` through
`round-10` output directories of this run:

| metric | before | after | change |
|---|---:|---:|---:|
| `count_words` median time | 44,702 ns | 38,636 ns | **−13.57 %** |

Every one of the 10 candidate-round medians (38,023–41,590 ns) measured
faster than every one of the 10 baseline-round medians (44,225–47,640 ns) —
complete separation between the two samples. The exact two-sided Mann-Whitney
p-value at that separation, for `n = 10` per side, is
`2 / C(20, 10) ≈ 1.08 × 10⁻⁵`, far under both the raw `alpha = 0.05` and the
Bonferroni-corrected bar (here undivided, since only one benchmark is
declared).

`score` — the geometric mean of `candidate_ns/baseline_ns`, and with one
declared benchmark, just that one ratio — was **0.8643**. That clears both
`KEEP` conditions: it is below `1 - min_effect_pct/100` (0.99 at the default
`min_effect_pct: 1.0`), and the improvement is significant past the
corrected bar. No benchmark regressed, so the third rule (`max_regress_pct`)
never comes into play with only one benchmark declared.

```
$ autor3search-rust eval --desc "with_capacity + push"
VERDICT: KEEP (improved) score=0.8643
score 0.8643 (-13.57%)
```

```
$ autor3search-rust report
autor3search-rust report: 1 experiment(s)  (1 keep, 0 discard, 0 fail, 0 crash)

cumulative speedup: 13.6%  (score 0.86, 1.16x faster overall)

largest wins:
  1. with_capacity + push (time: -13.6%)
```

**This run happened to reach KEEP.** It is reported exactly as it came out —
this project's standard is to report a DISCARD just as plainly if that had
been the result, not to retry until the tool produces the answer that looks
best. Every command's output above (`doctor`, `baseline`, `eval`, `report`)
is copied verbatim from the one run this case study describes; none of it
was edited or re-run to improve on the first result.

## Housekeeping and disk

This run was performed on a machine with roughly 2.1 GiB of free disk, which
`doctor` flagged. Disk usage after `eval` (which builds and benchmarks both
the candidate and the frozen baseline worktree, each with their own
`target/`) dropped to about 1.6 GiB free. The scratch repository's build
artifacts (`target/`) and the entire `AUTOR3SEARCH_RUST_STATE_HOME` scratch
directory (including the baseline worktree's own `target/` and criterion's
per-round sample data) were deleted immediately after the numbers above were
extracted, returning the machine to its starting ~2.1 GiB free.

This project's own test suite has a known disk-hygiene gap, and this run's
own verification stepped in it: `tests/pipeline_gates.rs` spins up a fresh
temporary git repository (with its own `target/`) per test, and an
interrupted run does not clean those up. Killing an in-progress
`cargo test --all-targets` invocation during this task (to keep free space
comfortably above the ~800 MiB floor treated as a hard stop) left two such
orphaned directories under the OS temp directory, totaling roughly 450 MB,
which had to be found and removed by hand before the freed space showed up
in `df`. Running the full `--ignored` end-to-end suite in parallel with
anything else on a similarly tight machine is not safe for the same reason.

## Bugs the review process caught

Following the Go case study's own example: this project records bugs its
implementation found in itself, not just the ones a user might. Three were
significant enough to be worth a permanent record, all caught by code review
before they shipped, in each case verified independently rather than taken
on the implementer's word.

### 1. `cargo metadata` does not emit a `harness` field at all

The bench-target discovery code originally classified a target as
criterion-driven by checking `t.harness == Some(false)` on the struct decoded
from `cargo metadata`'s JSON. On cargo 1.98, `cargo metadata --no-deps`
emits, per bench target, only
`[crate_types, doc, doctest, edition, kind, name, src_path, test]` — there is
no `harness` key at all, for either a `harness = false` or a `harness = true`
(auto-discovered, libtest) target. `Option<bool>` decoding a JSON object with
no such field simply produces `None`, silently, and `None == Some(false)` is
`false` — so **every** bench target was classified non-criterion, on every
repository, and `init`'s discovery loop would have driven zero benchmarks
into the run. This was caught in code review before it reached a test that
would have exercised it end to end, and the fix reads `harness` from the
package's `Cargo.toml` `[[bench]]` table directly (an absent `[[bench]]`
entry for an auto-discovered bench means `harness` defaults to `true`, i.e.
not criterion) via the package's `manifest_path`, which `cargo metadata` does
provide. Fixed in commit `6989b90` ("Fix: Read bench harness field from
Cargo.toml instead of cargo metadata"), adding the `toml` crate as the one
dependency this fix required.

### 2. `git status --porcelain` collapses a new untracked directory to just its name

The scope gate's `changed_since` used plain `git status --porcelain` to list
what a candidate commit touched. Git's default behavior for a brand-new,
entirely untracked *directory* is to report one line naming the directory
(`?? .cargo/`), not the files inside it — regardless of how many files that
directory contains. A regression test that created a fresh `.cargo/`
containing a new `config.toml` (exactly the `-C target-cpu=native` cheat
vector the locked-file gate exists to close — see
[What the harness enforces](../README.md#what-the-harness-enforces)) found
that `changed_since` reported only `.cargo/`, which matches neither
`locked_file`'s file-name check (which sees an empty path component after the
trailing slash) nor trips the scope matcher (`**` matches a bare directory
name too, so it would have been silently treated as an in-scope edit rather
than rejected). Concretely: an agent could have created `.cargo/config.toml`
with `RUSTFLAGS = "-C target-cpu=native"`, and the edit would have sailed
through scope, through config-hash, through a full real measurement, and
returned a verdict of `NoSignificantImprovement` rather than the
`ScopeViolation` it should have been rejected with before ever reaching the
compiler — a real, reproducible speedup attributed to a logic change that
never happened. Fixed with a one-line change: `git status --porcelain
--untracked-files=all`, which lists every file inside a new directory
individually rather than collapsing them. A permanent regression test,
`changed_since_reports_files_inside_a_new_untracked_directory`, now lives in
`gitx.rs`'s own test module.

### 3. A bare `#[cfg(test)]` item outside any `mod` was invisible to the inline-test hash

`freeze`'s inline-test hashing (`inline_hashes` / `collect_cfg_test`) is what
lets the harness refuse an edit to a `#[cfg(test)] mod tests { ... }` block
that lives inside a source file the agent is otherwise allowed to change
(whole `_test`-style files are frozen and restored byte for byte; inline
tests cannot be, without also erasing whatever real optimization sits next
to them in the same file). The first implementation of `collect_cfg_test`
walked the file's items looking only for `syn::Item::Mod` — so a bare
top-level `#[cfg(test)] fn test_helper() { ... }`, or a lone `#[cfg(test)]
#[test] fn foo() { ... }` sitting directly in the file outside any `mod`
block (both are valid, ordinary Rust) was skipped by the item loop entirely.
Such a file hashed identically whether or not that test function was gutted:
an agent could have deleted the body of a bare inline `#[test]` fn with zero
detection, the exact hole the whole mechanism exists to close. Fixed by
adding `item_attrs`, which extracts the attribute list from every `syn::Item`
variant that carries one (not just `Mod`), so `collect_cfg_test` now checks
every item in the file for a `#[cfg(test)]`-shaped attribute, not only
module declarations. Covered by
`a_bare_cfg_test_function_changing_changes_the_hash` and related tests in
`freeze.rs`.

(A related, narrower issue was found and fixed in the same review round:
`is_cfg_test`'s original implementation matched by checking whether the
attribute's token stream *contained the substring* `"test"` — which the
design's own prose had warned against, and which both false-positived on
`#[cfg(feature = "testing")]`-shaped attributes and, more seriously,
false-negatived by treating `#[cfg(not(test))]` — production-only code — as
test-gated. Replaced with a structural walk over the parsed `Meta` value that
returns `false` immediately inside a `not(...)`, and recurses into `all(...)`
and `any(...)` predicates rather than pattern-matching on rendered text.)

## What this run does and does not prove

This run, together with the project's own test suite — 240+ tests, including
end-to-end tests that drive `eval` to a real `KEEP` and a real `DISCARD`
against this same bundled demo crate — is the full extent of what has been
validated as of this writing. It has **not** been run against a third-party
crate from crates.io with an agent driving `program.md` end to end; that is
explicitly out of scope for this release (see
[Validation status, stated exactly](../README.md#validation-status-stated-exactly)
in the README) rather than silently skipped. A single `KEEP` on one small
benchmark is evidence that the pipeline works end to end on real code and
produces a real, checkable number — not evidence that the tool generalizes
to a large, unfamiliar
codebase with many benchmarks, nor a substitute for that run once the
machine and the time exist to do it properly.
