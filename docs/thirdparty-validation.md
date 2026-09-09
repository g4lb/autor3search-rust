# Third-party validation run: `unicode-segmentation` v1.13.3

This is the run the README's "Validation status" section says has not
happened yet: `autor3search-rust` driven end to end, by an agent following
`templates/program.md` literally, against a real crate from crates.io that
the tool was never designed with in mind. the harness repository
itself was **not modified** — everything below happened in a scratch clone,
built from the harness's own `target/release/autor3search-rust` binary
(`autor3search-rust 0.1.0 (f394afa)`).

Machine: macOS 26.6.2, 10 logical CPUs, on AC power, `rustc 1.98.0`,
`cargo 1.98.0`, `samply 0.13.1`.

Scratch location: `<scratch dir>`.

> **What happened to the findings.** All three defects this run turned up —
> `init` not seeing criterion 0.3.x/0.4.x benchmarks, `profile` failing
> against samply 0.13, and the doc-comment gate firing on plain prose — were
> fixed after it, with regression tests, and the fixes were verified against
> the same crates named below. The report is left as it was written, so the
> findings and the evidence for them stay legible; read it as the record of
> a run, not as a list of current defects.

## tl;dr

- The loop worked end to end: `init` → `doctor` → `baseline` → four real
  hypothesis-driven experiments → `report`, with no crash and every exit
  code, reason string, and log row matching what the README and
  `program.md` promise.
- Two real `KEEP`s, two honest `DISCARD`s. The biggest win
  (**-90.53%** on one benchmark, geomean **-55.19%**) was believable and
  explainable line-by-line; the two `DISCARD`s were changes I predicted
  might not move the needle, and they didn't.
- **`autor3search-rust init` has a real discovery bug**: it cannot find
  benchmarks written against criterion 0.3.x/0.4.x, because those versions'
  `--list` output ends lines in `: bench`, not `: benchmark`, and the
  parser only matches the latter. This silently misreports "no benchmarks
  found" on a crate that has real, working criterion benchmarks. Found via
  `httparse` and `bytecount`, both real, widely-used crates.
- **`autor3search-rust profile` crashes (exit 2) against the current,
  `cargo install`-able version of samply** (0.13.1), because samply's
  profile JSON now uses a per-thread `stringArray` field where the harness's
  parser still expects a top-level `stringTable`. The documented graceful
  path only covers "samply is not installed" — it does not cover "samply is
  installed and its output format moved on."
- **A real gate over-reach**: the inline-test/doctest freeze hashes *every*
  doc comment in a source file as a single blob, not just fenced doctest
  code. Adding a brand-new, plain-prose `///` comment on brand-new code —
  zero doctest content, nothing resembling a test — trips the same
  `FAIL(inline_test_modified)` as weakening a real doctest. This is a real
  restriction the docs don't warn about: an agent cannot document new code
  in a file that also has doctests, for the rest of the run.
- Deliberately tried to defeat the harness (weakened a frozen test file's
  assertions and broke the implementation it was supposed to catch, in the
  same commit) specifically to check the "frozen tests are restored" claim.
  It held: `eval` still reported `FAIL(tests_failed)` against the *original*
  assertions, and the working tree showed the frozen file restored on disk
  afterward, diffing away from my own commit.
- `report`'s "cumulative speedup" is, to the decimal, the product of the two
  kept scores (`0.9573 × 0.4481 = 0.4290`, i.e. `-57.1%`, `2.33x`) — exactly
  as documented, not something I had to take on faith.

## Why `unicode-segmentation`

The task's suggested candidates were checked first, and none of them
actually worked:

| Crate | What I found |
|---|---|
| `strsim` | `#[bench]` + `#![feature(test)]` (nightly libtest), not criterion at all. |
| `humantime` | `harness = false` benches exist, but they use `bencher`, not criterion. |
| `heck` | No `benches/` directory at all. |
| `bytecount` | Real criterion (0.4.0) benches, but `init` reports "no benchmarks found" — see the bug below. Also fans out into ~220 benchmark ids (one per input size × function), impractically slow to measure. |
| `httparse` | Real criterion (0.3.6, via a `"0.3.5"` requirement) benches, clean 9-id set, zero runtime dependencies, `[profile.bench]` override (a nice `doctor` test) — but `init` reports "no benchmarks found" for the same reason as `bytecount`. This is what led me to the criterion-version bug (see below). |

I picked **`unicode-segmentation` v1.13.3** (`unicode-rs/unicode-segmentation`)
after confirming: criterion `"0.5"` (modern `--list` format `init` can
actually parse), **zero runtime dependencies** (only `criterion`,
`quickcheck`, `proptest` as dev-dependencies), a full `cargo build --release
--benches` in **14 s**, real algorithmic code (Unicode grapheme/word/sentence
boundary segmentation — table lookups, a hand-written state machine for
UAX#29 word-break rules, an existing ASCII fast path for some but not all of
its iterators), and four separate criterion targets so a real hypothesis
could target one code path without dragging in the others. It is also a
crate maintained by Unicode/rust-lang veterans and already carries hand-written
`#[inline]` hints and ASCII fast paths in most (but, as it turned out, not
all) of the hot places — exactly the "already reasonably tuned" shape the
task asked me to be honest about rather than to manufacture wins against.

## The criterion `--list` output-format bug (found via `httparse`/`bytecount`)

`autor3search-rust init` on both `httparse` (criterion 0.3.6) and
`bytecount` (criterion 0.4.0) printed:

```
no benchmarks found: this tool optimizes what it can measure, and refuses to guess.
```

despite `cargo metadata` correctly finding the `[[bench]] harness = false`
target in both cases. Running the exact command `init` uses,
`cargo bench --bench <name> -- --list`, by hand showed why:

```
$ cargo bench --bench parse -- --list        # httparse, criterion 0.3.6
header/count_008: bench
version/http10: bench
method/get: bench
...
```

`src/discover.rs`'s `parse_list_output` does:

```rust
stdout
    .lines()
    .filter_map(|line| line.trim_end().strip_suffix(": benchmark"))
```

Criterion 0.3.x and 0.4.x's `--list` ends each line in `": bench"`, not
`": benchmark"` — I confirmed the format actually changed by bumping
`httparse`'s dev-dependency to `criterion = "0.5"` in my scratch clone only
(reverted immediately after) and re-running the identical command:

```
$ cargo bench --bench parse -- --list        # httparse, criterion pinned to 0.5.1
method/w3!rd: benchmark
many_requests/_: benchmark
```

So the discovery parser silently misreports "no benchmarks" for any crate
still pinned to criterion <0.5 — which is common; `httparse`'s and
`bytecount`'s pins are not exotic. This is a real false negative in `init`,
not a fixture-only edge case: the benchmarks are real, `cargo bench` runs
them fine, and a user would have no way to know from the error message that
their criterion version, not their benchmarks, is the problem.

## `doctor`

```
[ok  ] cpu frequency scaling    not checked on this platform
[ok  ] thermal throttle risk    not checked: could not parse `pmset -g therm`
[ok  ] load average             1-minute load average 4.33 (threshold 5.00 for 10 logical cpus)
[ok  ] disk space               662.9 GB available
[ok  ] power source             on AC power
[ok  ] CARGO_INCREMENTAL        not set
[ok  ] [profile.bench]          opt-level not overridden
[ok  ] default toolchain        rustc 1.98.0 (88d9e12ae 2026-08-18)
```

Exit code `0`, as documented. Every line is a genuine, correct read of this
machine (no thermal API on this macOS build, load average and disk space
correctly reported, no bench-profile override in `unicode-segmentation`'s
`Cargo.toml`).

## `init`, and the benchmark selection I made as the human

`init` found all **43** real benchmark ids across the crate's four bench
targets (`chars`, `word_bounds`, `words`, `unicode_word_indices` — one per
grapheme-cluster/word-boundary API, each run against 8 language samples).
It also correctly appended to an *existing* `.gitignore` rather than
clobbering it (the crate already had `target`, `Cargo.lock`,
`scripts/tmp`, `*.pyc`, `*.txt` in there; `init` added its own block below
them, untouched).

As the human, I trimmed `benchmarks:` before `baseline`, for two reasons —
this is exactly the kind of judgment call `.autor3search/config.yaml` being
human-owned is for, and I want to be explicit about it since it affects how
much weight every verdict below carries:

1. **Two of the crate's own benchmark "variants" are stdlib controls, not
   crate code.** `chars`' and `words`' `scalar` variants call
   `str::chars()`/`str::split_whitespace()` directly — no
   `unicode-segmentation` code runs at all. No edit inside `scope` (`src/**`)
   could ever move them; including them would only add dead weight to the
   geomean and an unearned extra benchmark to the Bonferroni correction.
2. **43 benchmark ids at the default `count: 10` would make every `eval`
   take a very long time** (each id pays its own warm-up + measurement
   time regardless of how fast the function under test is).

Final selection, all of which do run crate code: `chars/grapheme/english`,
`word_bounds/grapheme/source_code`, `words/grapheme/english` — one id per
distinct iterator family, one non-ASCII-heavy sample and one 100%-ASCII
sample, so a hypothesis limited to the ASCII path could be told apart from
one that touches the general Unicode path.

**A mistake I made and the harness caught immediately:** my first pass at a
"fast" config used `count: 4` (this repo's `MIN_COUNT`) with these 3
benchmarks. The very first `eval` returned, correctly:

```json
{"status":"DISCARD","reason":"no_significant_improvement",
 "warnings":["confidence interval requires at least 6 observations at 95%
 confidence; got 4, so the interval is unbounded","no KEEP was reachable:
 comparing 3 benchmark(s) corrects the significance threshold to 0.01667,
 but with 4 rounds per side the test cannot produce a p-value below 0.02857
 however large the improvement is — raise count to at least 5"]}
```

This is `program.md`'s own documented warning ("no KEEP was reachable...
treat this as a broken configuration and stop"), and it fired exactly when
it should have — with `k=3` and `count=4`, the exact two-sided Mann-Whitney
minimum p-value (2/C(8,4) = 0.02857) can never clear the Bonferroni bar
(0.05/3 = 0.01667), so every experiment under that config was mathematically
guaranteed to `DISCARD` regardless of the real effect size. I reset the
commit, raised `count` to `6` (2/C(12,6) = 0.00216 clears 0.01667 with
room to spare), rebaselined, and reran that same experiment cleanly. **This
is the harness behaving exactly as documented** — it told me my own config
was broken instead of quietly producing false negatives all night.

Final config used for every real experiment below:

```yaml
benchmarks:
- chars/grapheme/english
- word_bounds/grapheme/source_code
- words/grapheme/english
count: 6
sample_size: 10
measurement_time: 500ms
warm_up_time: 200ms
max_regress_pct: 5.0
min_effect_pct: 1.0
timeout: 15m
```

(`bench_targets` kept all four discovered targets — narrowing `benchmarks:`
is enough; `eval` builds a criterion filter regex from `benchmarks:` and
skips non-matching ids cheaply within each target.)

## `baseline`

```
run tag        v4
branch         autor3search-rust/v4  (checked out)
baseline       48e9256
frozen         6 test file(s), 5 source file(s) hashed for inline tests
worktree       <local path>
benchmarks     chars/grapheme/english, word_bounds/grapheme/source_code, words/grapheme/english
```

(This crate went through three `baseline` tags in total — `v1`/`v2` were my
own false starts, abandoned before any real experiment, first because my
initial benchmark selection accidentally included the stdlib-control ids,
then because of the `count: 4` mistake above. `v4` is the run every number
below comes from. Nothing about restarting a baseline before any experiment
runs against it required `--force` or touched anything the harness owns.)

## `profile`

Without `samply` installed:

```
samply is not installed, so there is nothing to profile with.

samply is chosen over cargo-flamegraph because it needs no elevated
privileges — dtrace on macOS requires disabling SIP.

Install it with:

    cargo install samply

then run 'autor3search-rust profile' again.
```

Exit code `0` — matches the README exactly ("prints how to install it and
exits 0 rather than failing the run").

After `cargo install samply` (installed `samply 0.13.1`, the current
crates.io release):

```
$ autor3search-rust profile
parse .../v1/profiles/unicode-segmentation-chars.json: decode profiler JSON: missing field `stringTable` at line 1 column 386086
```

Exit code **2** — a real crash, not the documented graceful path. The
profile JSON samply 0.13.1 actually writes has moved the string table off
the top level and into each thread (`threads[0].stringArray`, confirmed by
loading the file and listing its keys: `meta, libs, threads, pages,
profilerOverhead, counters` at top level, and `frameTable, funcTable,
markers, name, ..., stringArray, ...` per thread) — `src/profile.rs`'s
`FirefoxProfile` struct still expects a `#[serde(rename = "stringTable")]`
field and there is no fallback. So `profile`'s documented degradation only
covers "samply absent"; "samply present, but a newer samply" is an
unhandled crash, on the exact version `cargo install samply` gives you
today.

## Experiment 1 — inline the `grapheme_category` lookup chain

**Hypothesis.** `tables::grapheme::grapheme_category` and its private
`bsearch_range_value_table` helper are called on every non-ASCII character
`GraphemeCursor` sees (ASCII already has its own fast branch one level up,
in `GraphemeCursor::grapheme_category`), but neither carries `#[inline]`,
unlike almost everything else in the hot grapheme path. Cross-module
inlining across codegen units is a hint, not a given, so adding it might let
LLVM fold the table lookup into the caller.

**Change** (`src/tables.rs`, two `+#[inline]` lines, no logic change):

```diff
+    #[inline]
     fn bsearch_range_value_table(c: char, r: &[(char, char, GraphemeCat)], ...
...
+    #[inline]
     pub fn grapheme_category(c: char) -> (u32, u32, GraphemeCat) {
```

**Verdict:**

```json
{"status":"DISCARD","reason":"no_significant_improvement",
 "score":1.0024750328727752,"message":"score 1.0025 (+0.25%), no significant improvement"}
```

`git reset --hard HEAD~1` applied.

**Was it believable?** Yes. Two of my three benchmarks
(`word_bounds/grapheme/source_code`, `words/grapheme/english`) never call
`grapheme_category` at all — only `chars/grapheme/english` does, and only
for its handful of non-ASCII characters (the sample is mostly-ASCII English
prose). A ~0.25% wash across a 3-benchmark geomean, with the only relevant
benchmark contributing a tiny effect diluted by two unrelated ones, is
exactly what I'd expect from this change touching a rarely-hit branch.

## Experiment 2 — inline the `word_category` lookup chain

**Hypothesis.** The same missing `#[inline]` pattern, but on
`tables::word::word_category` — which, unlike `grapheme_category`, has
**no** ASCII short-circuit above it and is called on essentially every
character `UWordBounds::next()`/`next_back()` see (directly, and again via
its own `get_next_cat`/`get_prev_cat` peek-ahead helpers). This should matter
far more.

**Change** (`src/tables.rs`, same shape, word module this time):

```diff
+    #[inline]
     fn bsearch_range_value_table(c: char, r: &[(char, char, WordCat)], ...
...
+    #[inline]
     pub fn word_category(c: char) -> (u32, u32, WordCat) {
```

**Verdict:**

```json
{"status":"KEEP","reason":"improved",
 "score":0.9572708458288324,"message":"score 0.9573 (-4.27%)"}
```

`results.tsv`: `c2d91b9  0.9573  -7.44  KEEP`. Commit kept; measurement
baseline advanced to `c2d91b9`.

**Was it believable?** Yes, and it is the more interesting of the two
`#[inline]` experiments precisely because it split from experiment 1 the way
I predicted: this function sits on the hottest path in the file for two of
my three benchmarks, and a real, if modest, -4.27% geomean / -7.44%
best-single-benchmark win for a two-line, purely-additive, zero-risk change
is a believable amount for "the compiler wasn't inlining a small function it
plausibly should have."

## Experiment 3 — ASCII fast path for `split_word_bounds`/`split_word_bound_indices`

**Hypothesis, from reading the code, not a guess.** `unicode_words()` and
`unicode_word_indices()` already dispatch to a much cheaper
`AsciiWordBoundIter` for all-ASCII input (`new_unicode_words`/
`new_unicode_word_indices` in `src/word.rs` check `s.is_ascii()` up front).
`split_word_bounds()`/`split_word_bound_indices()` — built on the same
`UWordBounds` struct — have no such check and always run the full
per-character UAX#29 state machine, even on text that is 100% ASCII. This is
exactly the benchmark `word_bounds/grapheme/source_code` exercises (the
crate's own, entirely-ASCII, source file as sample text).

**Change** (`src/word.rs`, +104 lines): added a `is_ascii: bool` field to
`UWordBounds`, set once in `new_word_bounds` from `s.is_ascii()`; added two
small private functions, `ascii_word_run_len`/`ascii_word_run_len_back`,
that are a byte-for-byte port of `AsciiWordBoundIter::next`/`next_back`'s
existing, already-tested classification rules (literally calling its
private `is_core`/`is_infix` predicates) but returning just a run length
instead of mutating a separate `rest`/`offset` pair; and a short-circuit at
the top of `UWordBounds::next`/`next_back` that uses them when
`self.is_ascii`. No existing code path was touched — the general algorithm
is untouched and still runs for any non-ASCII input.

**First attempt failed on a gate, not on correctness** — see the
"doc-comment over-reach" section below; I wrote real `///` doc comments on
the new field and functions, and `eval` returned:

```json
{"status":"FAIL","reason":"inline_test_modified",
 "message":"inline tests changed in [\"src/word.rs\"] — a #[cfg(test)] module or a doctest inside a source file you may otherwise edit. These cannot be restored for you, because restoring the file would erase your optimization with them. Revert the test to its baseline form and rerun."}
```

`results.tsv`: `70e75cc  0.0000  0.00  FAIL`. I converted the new comments
from `///` to `//` (content unchanged, nothing about the code changed),
reran the crate's own test suite locally first (`cargo test`: **all 40
tests pass**, including two property tests generating random ASCII strings
0–99 chars long and asserting the new fast path's output equals the general
algorithm's, forward and reversed — `proptest_ascii_matches_unicode_word_indices`
and its `_rev` counterpart, both pre-existing, both exercising exactly the
code path I changed), then recommitted.

**Verdict:**

```json
{"status":"KEEP","reason":"improved",
 "score":0.4481330722261249,"message":"score 0.4481 (-55.19%)"}
```

`results.tsv`: `6f38e9b  0.4481  -90.53  KEEP`. Commit kept; measurement
baseline advanced to `6f38e9b`.

**Was it believable?** Very. Only `word_bounds/grapheme/source_code`
(100% ASCII) can take the new fast path; `chars/grapheme/english` is a
different module entirely (grapheme.rs, untouched) and `words/grapheme/english`
uses non-ASCII sample text (English prose has smart quotes), so it still
takes the *general* path exactly as before. One of three benchmarks dropping
by **-90.53%** (bypassing an entire per-character state machine for simple
byte classification) while the other two sit near 1.0 produces a geomean of
roughly `(0.095 × 1 × 1)^(1/3) ≈ 0.455`, which lines up with the measured
0.4481. This is the single most convincing result of the run: a real
algorithmic gap, found by reading the code and noticing an inconsistency
between two sibling functions, reusing already-tested logic to close it, and
a verdict whose size and shape match the mechanism exactly.

## Experiment 4 — escalate `check_pair` to `#[inline(always)]`

**Hypothesis.** `grapheme.rs`'s `check_pair` (the GB3–GB13 pairwise
boundary-rule match, called on essentially every grapheme boundary decision)
already carries plain `#[inline]`. Forcing `#[inline(always)]` is a
different, independently testable hypothesis — a stronger hint might still
change codegen even where a hint is already present, in either direction
(faster from more inlining, or slower from code bloat).

**Change** (`src/grapheme.rs`, one line):

```diff
-#[inline]
+#[inline(always)]
 fn check_pair(before: GraphemeCat, after: GraphemeCat) -> PairResult {
```

**Verdict:**

```json
{"status":"DISCARD","reason":"no_significant_improvement",
 "score":0.9900280026890854,"message":"score 0.9900 (-1.00%), no significant improvement"}
```

`results.tsv`: `c025d26  0.9900  -1.31  DISCARD`. `git reset --hard HEAD~1`
applied.

**Was it believable, and was the *reason* code right?** Yes on both counts.
The geomean (0.9900) sits fractionally above the KEEP threshold
(`1 - 1%/100 = 0.99`, and 0.990028 is not below that), so rule 1 alone would
have blocked a KEEP — but the reported reason was `no_significant_improvement`,
not `improvement_below_min_effect`. Reading `src/verdict.rs::decide`
confirmed why: `improvement_below_min_effect` only fires when at least one
individual benchmark cleared the Bonferroni-corrected significance bar
(`p < alpha/k`) *and* the overall score still missed the effect floor; here,
no single benchmark cleared that bar at all, so the harness correctly
reported "nothing measurably moved" rather than overstating a noise-level
wobble as "a real improvement, just a small one." An already-`#[inline]`d,
small match expression having no further measurable benefit from a stronger
hint is exactly the outcome I expected going in.

## Doc-comment gate over-reach (found while building experiment 3)

`src/freeze.rs`'s `inline_hashes`/`collect_docs` hashes **every** doc
comment in a source file — `///` and `#[doc = ...]` — as one concatenated
blob, with no check for whether the comment actually contains a fenced code
block. Confirmed directly in the source:

```rust
fn collect_docs(attrs: &[syn::Attribute], out: &mut String) {
    for attr in attrs {
        if !attr.path().is_ident("doc") { continue; }
        // ...
        out.push_str(&s.value());
        out.push('\n');
```

There is no filtering for ` ``` ` fences here — a plain-prose `///` comment
on a *brand-new* private function, containing zero test code, changes this
hash exactly like weakening a real doctest would. I hit this directly: my
first version of experiment 3 added ordinary explanatory `///` comments to
new code and got `FAIL(inline_test_modified)` even though nothing that could
possibly be a doctest existed before or after my change. Converting the same
comments to plain `//` (not a doc comment in Rust, so `syn` never sees a
`doc` attribute for it) made the identical functional diff pass cleanly.

This is worth stating plainly because it is not what the README or
`program.md` describe. Both explain this gate as protection against
*weakening* an existing doctest ("Weaken a doctest | ... doctest text is
hashed at baseline, and any change to either is `FAIL`"). What it actually
does is broader: for the rest of a run, once a source file has any doctest
in it at baseline, an agent cannot add a single new documented function to
that file — not "cannot weaken a test," but "cannot write a doc comment,
full stop," including on code that has nothing to do with any test. On a
crate like this one, where nearly every public and many private items carry
`///` docs, that is a real, silent tax on how an agent should write code
during a run: plain `//` comments work fine; `///` on anything new does not,
and the failure message ("a doctest inside a source file...") does not
mention that the trigger can be a non-doctest doc comment at all.

## Verifying "frozen tests are restored," on purpose

This was not one of the four hypothesis experiments — it is a deliberate,
one-off attempt to defeat the harness, done and reverted immediately, kept
here because the coordinator specifically asked whether this claim holds.

In one commit: (1) neutered `tests/test.rs`'s `assert_!` macro (used by
`test_words`, which directly calls `split_word_bounds()` — exactly the code
path experiment 3 touches) into a no-op that only calls `.collect()` and
discards the result; (2) broke `AsciiWordBoundIter::is_core` to always
return `false`, which breaks ASCII word splitting outright. Verified locally
first that this combination is coherent: `cargo test --test test` on this
sabotaged tree reports **all tests passing** (the broken implementation
is real, but the weakened test can no longer see it).

Committed both changes together and ran `eval`:

```json
{"status":"FAIL","reason":"tests_failed",
 "message":"... error: test failed, to rerun pass `--lib`"}
```

And, crucially, immediately after that `eval` returned:

```
$ git status
On branch autor3search-rust/v4
Changes not staged for commit:
        modified:   tests/test.rs
```

— `tests/test.rs` on disk no longer matched my own committed sabotage; the
harness had silently overwritten it with the frozen original before
building and testing, which is what caught the deliberately broken
`is_core` for real. Both halves of the documented claim held: the frozen
copy governs what `eval` actually measures against, not what's committed,
and it does so by restoring the working tree (not by comparing hashes and
refusing) so the exact original assertions run every time. `git reset
--hard HEAD~1` immediately after, per the same rule as any other
non-`KEEP` verdict; this row is not counted among the four real experiments.

## `report`

```
autor3search-rust report: 6 experiment(s)  (2 keep, 2 discard, 2 fail, 0 crash)

cumulative speedup: 57.1%  (score 0.43, 2.33x faster overall)

largest wins:
  1. ascii fast path for split_word_bounds (reuse AsciiWordBoundIter rules) (time: -90.5%)
  2. inline word_category + bsearch helper (time: -7.4%)
```

Exit code `0`. Checked the arithmetic by hand rather than taking the
"product of kept scores" claim on faith:

```
0.9572708458288324 × 0.4481330722261249 = 0.42898...
1 - 0.42898  = 0.57102  → 57.1%   ✓ matches "cumulative speedup: 57.1%"
1 / 0.42898  = 2.3311   → 2.33x   ✓ matches "2.33x faster overall"
```

The two `FAIL` rows (the doc-comment misfire and the deliberate sabotage
attempt) correctly contribute nothing to the cumulative product — only kept
scores multiply in — and both still show up in the experiment count (6 run:
2 keep, 2 discard, 2 fail) and in `results.tsv`, exactly as `program.md`
describes ("a long trail of honest discards is more useful to the human
than a short trail that hides them").

Final `status`:

```
run tag        v4
branch         autor3search-rust/v4  (checked out)
baseline       48e9256  (run started here)
measuring vs   6f38e9b  (advanced past the baseline by earlier KEEPs)
worktree       <local path>
experiments    6 run  (2 keep, 2 discard, 2 fail, 0 crash)  — next is #7
stop           not requested
```

## What I did not test

- **`stop` / `stop --force` mid-`eval`.** Every experiment here finished in
  under a minute at the fast config, so there was never a long-running
  `eval` worth interrupting. Not exercised, not verified in this run.
- **Windows.** This machine is macOS only; the README already states the
  Windows `stop --force` path is compiled but not executed by CI, and I
  have nothing to add to that.
- **`unsafe`-based cheating.** I did not attempt this — the README already
  states plainly that no gate exists against it in this release, and I saw
  no reason to demonstrate what is already documented as an open gap.

## Everything that behaved differently from what the docs promise

1. **`init` cannot discover benchmarks on criterion <0.5** (found via
   `httparse`, `bytecount`) — `parse_list_output` matches `": benchmark"`
   but those versions emit `": bench"`. Silent false negative, reported as
   "no benchmarks found" with no hint that the cause is a criterion version,
   not an absence of benchmarks.
2. **`profile` crashes (exit 2), not exits 0, when samply is installed but
   newer than the harness's parser expects** — confirmed against
   `samply 0.13.1`, the version `cargo install samply` currently gives you,
   due to `stringTable` (harness) vs `stringArray` (samply's current
   per-thread field) in the profile JSON schema.
3. **The doctest-freeze gate is broader than documented**: it hashes *every*
   doc comment in a file, not just fenced doctest code, so writing a
   plain-prose `///` comment on brand-new code in a file that has doctests
   trips the same `FAIL(inline_test_modified)` as weakening a real test.
   `program.md`/README describe this gate only in terms of weakening an
   existing doctest.

Everything else held exactly as documented: `doctor`'s exit-0 informational
contract; `baseline`'s dirty-tree and reused-tag refusals; the
"no KEEP reachable" configuration guard firing precisely on the math it
claims to check; every `eval` exit code (0/1/2) and `reason` string matching
`program.md`'s table; the measurement baseline advancing to the
just-kept commit and not before; `results.tsv` recording every experiment,
KEEP and not; frozen test files actually being restored under a real
adversarial test, not just claimed; and `report`'s cumulative figure being
the exact product of kept scores, checked by hand against the printed
percentage and multiplier.
