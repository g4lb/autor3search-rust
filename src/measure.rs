//! Collects benchmark observations from a baseline and a candidate tree in an
//! interleaved order.
//!
//! This is the core measurement discipline. Comparing a candidate measured now
//! against a baseline measured minutes ago attributes CPU thermal drift,
//! frequency scaling and background load to the code change. Alternating the
//! two sides within a single session cancels that drift, because both sides
//! experience the same conditions.

use crate::bench::{self, Set};
use crate::config::BenchTarget;
use crate::runner::Runner;
use crate::state::CANCEL_REQUESTED;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

/// A distinguishable error string [`run`]'s caller checks for before falling
/// through to its ordinary "measurement failed" handling.
///
/// A sentinel string rather than a richer error type: `interleave` and
/// `side` already return a plain `Result<_, String>` used by tests with
/// bare closures that know nothing about cancellation, and threading a new
/// enum through that public shape (and every existing caller and test of
/// it) would be a far bigger change than this one property needs. Matching
/// on the message is unusual, but the two spots that produce and consume it
/// are a few lines apart in this module and `pipeline.rs`, so it cannot
/// drift silently.
pub const CANCELLED_SENTINEL: &str = "__autor3search_measurement_cancelled__";

/// Runs both sides alternately and accumulates their observations.
///
/// The two sides **swap order on every round** — base,cand then cand,base —
/// rather than always running base first. Alternating rounds alone cancels
/// drift *between* rounds, but a fixed order within each round leaves a
/// systematic offset: the candidate is then always measured one slot later, so
/// drift that is monotonic across a round (a CPU still ramping toward its
/// thermal steady state, a background job starting mid-run) lands on the
/// candidate in the same direction every single time. Averaging over rounds
/// does not remove it, because it is not noise — it is a constant bias, and it
/// shifts the very score the KEEP threshold is compared against. Swapping
/// makes each side occupy the first slot half the time, cancelling that term
/// to first order.
///
/// An odd number of measured rounds cannot be split evenly and leaves one
/// round's worth of the offset behind; an even count — the default is 10 —
/// cancels it exactly.
///
/// When `warmup` is true an extra leading round is run and discarded,
/// absorbing first-touch effects such as cold caches and a cold build.
pub fn interleave(
    rounds: usize,
    warmup: bool,
    base: &mut dyn FnMut(usize) -> Result<Set, String>,
    cand: &mut dyn FnMut(usize) -> Result<Set, String>,
) -> Result<(Set, Set), String> {
    if rounds < 2 {
        return Err(format!("need at least 2 measured rounds, got {rounds}"));
    }
    let total = if warmup { rounds + 1 } else { rounds };
    let mut base_set = Set::new();
    let mut cand_set = Set::new();

    for i in 0..total {
        // Checked BETWEEN rounds, not mid-round: `side`'s own subprocess
        // call already reacts within one poll tick if the flag becomes true
        // while a round is actually running (see `Runner::cargo`), so this
        // catches the gap right after a round finishes cleanly and before
        // committing to another one — no experiment's worth of benchmarking
        // is ever wasted on a round that will not count.
        if CANCEL_REQUESTED.load(Ordering::SeqCst) {
            return Err(CANCELLED_SENTINEL.to_string());
        }
        let (b, c) = one_round(i, base, cand)?;
        if warmup && i == 0 {
            continue;
        }
        base_set.add(&b);
        cand_set.add(&c);
    }
    Ok((base_set, cand_set))
}

/// Runs both sides once — baseline first on even rounds, candidate first on
/// odd ones — and returns them in `(baseline, candidate)` order however they
/// ran. See [`interleave`] for why the order alternates.
fn one_round(
    i: usize,
    base: &mut dyn FnMut(usize) -> Result<Set, String>,
    cand: &mut dyn FnMut(usize) -> Result<Set, String>,
) -> Result<(Set, Set), String> {
    if i % 2 == 0 {
        let b = base(i).map_err(|e| format!("baseline round {i}: {e}"))?;
        let c = cand(i).map_err(|e| format!("candidate round {i}: {e}"))?;
        Ok((b, c))
    } else {
        let c = cand(i).map_err(|e| format!("candidate round {i}: {e}"))?;
        let b = base(i).map_err(|e| format!("baseline round {i}: {e}"))?;
        Ok((b, c))
    }
}

/// Builds a criterion filter regex matching exactly these benchmarks.
///
/// An empty list yields `.`, meaning every benchmark. Each name is escaped:
/// names from `--list` need no escaping, but `benchmarks:` in `config.yaml` is
/// documented as hand-editable, and a stray metacharacter in a hand-typed name
/// would otherwise silently *broaden* the pattern to match benchmarks nobody
/// selected.
pub fn bench_filter(names: &[String]) -> String {
    if names.is_empty() {
        return ".".to_string();
    }
    let escaped: Vec<String> = names.iter().map(|n| escape_regex(n)).collect();
    format!("^({})$", escaped.join("|"))
}

/// Escapes regex metacharacters. `/` is deliberately left alone: criterion's
/// grouped benchmark ids contain it and it is not a metacharacter.
fn escape_regex(s: &str) -> String {
    const SPECIAL: &[char] = &[
        '.', '^', '$', '*', '+', '?', '(', ')', '[', ']', '{', '}', '|', '\\',
    ];
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if SPECIAL.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Configures [`run`].
pub struct Options {
    /// The pinned baseline worktree and the repository under test.
    pub base_dir: PathBuf,
    pub cand_dir: PathBuf,
    /// The criterion bench targets to invoke, package-qualified.
    pub targets: Vec<BenchTarget>,
    /// The declared benchmark set. Every name must appear in every round.
    pub benchmarks: Vec<String>,
    pub sample_size: usize,
    pub measurement_secs: f64,
    pub warm_up_secs: f64,
    pub rounds: usize,
    pub warmup: bool,
    pub timeout: Duration,
    /// Where per-round criterion output is written, out of tree.
    pub criterion_root: PathBuf,
}

/// Measures one side once, for one round, across every configured target.
///
/// Pulled out of [`run`] as a free function (rather than a closure shared by
/// both sides) because two closures each borrowing a shared `side` closure
/// mutably is rejected by the borrow checker. A free function taking
/// everything it needs by parameter has no such borrow to share; `log` is
/// threaded through a `RefCell` for the same reason — `base_fn` and `cand_fn`
/// both need to reach it, and `interleave` never calls them at the same time,
/// so the borrow is never actually contended, only shared.
fn side(
    o: &Options,
    log: &std::cell::RefCell<Option<&mut dyn Write>>,
    label: &'static str,
    dir: &Path,
    round: usize,
) -> Result<Set, String> {
    let filter = bench_filter(&o.benchmarks);
    let mut round_set = Set::new();
    for t in &o.targets {
        let home = o
            .criterion_root
            .join(label)
            .join(format!("round-{round}"))
            .join(&t.package)
            .join(&t.target);
        // A FRESH directory per round is load-bearing. Criterion rotates
        // new/ into base/ on a second run in the same directory, so a
        // round whose benchmark failed to run would leave the PREVIOUS
        // round's estimates.json sitting in new/ and the harness would
        // score a stale number as if it were fresh.
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home)
            .map_err(|e| format!("create criterion home {}: {e}", home.display()))?;

        let sample = o.sample_size.to_string();
        let mtime = format!("{}", o.measurement_secs);
        let wtime = format!("{}", o.warm_up_secs);
        let args = vec![
            "bench",
            "-p",
            t.package.as_str(),
            "--bench",
            t.target.as_str(),
            "--",
            "--noplot",
            "--sample-size",
            sample.as_str(),
            "--measurement-time",
            mtime.as_str(),
            "--warm-up-time",
            wtime.as_str(),
            filter.as_str(),
        ];
        let runner =
            Runner::new(dir, o.timeout).with_env("CRITERION_HOME", &home.to_string_lossy());
        let out = {
            let mut log_ref = log.borrow_mut();
            // Reborrow manually rather than `as_deref_mut()`: through a
            // `RefMut`, that method ties the output's lifetime to the whole
            // `RefCell` borrow rather than to this block, which the borrow
            // checker then refuses to let end before the enclosing loop's
            // next iteration re-borrows it.
            let inner: Option<&mut dyn Write> = match log_ref.as_mut() {
                Some(w) => Some(&mut **w),
                None => None,
            };
            runner.cargo(&args, inner)?
        };
        if out.cancelled {
            return Err(CANCELLED_SENTINEL.to_string());
        }
        if out.timed_out {
            return Err(format!(
                "benchmark round timed out after {:?} in {}",
                o.timeout,
                dir.display()
            ));
        }
        if !out.ok() {
            return Err(format!(
                "benchmark round failed in {} (exit {}):\n{}",
                dir.display(),
                out.exit_code,
                out.tail(30)
            ));
        }
        round_set.add(&bench::read_round(&home)?);
    }

    // A criterion filter that matches nothing EXITS 0 AND PRINTS NOTHING —
    // verified during design. Without this check a typo'd or stale
    // benchmark name would measure nothing, produce no error, and leave
    // the harness scoring whatever it happened to find.
    let missing: Vec<&String> = o.benchmarks.iter().filter(|n| !round_set.has(n)).collect();
    if !missing.is_empty() {
        return Err(format!(
            "no results for benchmark(s) {missing:?} in {} — the criterion filter {filter} \
             matched nothing, which exits 0 silently. Check the names in \
             .autor3search/config.yaml against `cargo bench -- --list`.",
            dir.display()
        ));
    }
    if round_set.is_empty() {
        return Err(format!(
            "no benchmarks matched {filter} in {}",
            dir.display()
        ));
    }
    Ok(round_set)
}

/// Measures both worktrees with real cargo bench invocations.
pub fn run(o: &Options, log: Option<&mut dyn Write>) -> Result<(Set, Set), String> {
    if o.targets.is_empty() {
        return Err("no criterion bench targets configured".to_string());
    }
    let log = std::cell::RefCell::new(log);

    // Two closures over the same free function, so the interleaver stays
    // ignorant of how a side is measured.
    let mut base_fn = |r: usize| side(o, &log, "baseline", &o.base_dir, r);
    let mut cand_fn = |r: usize| side(o, &log, "candidate", &o.cand_dir, r);
    interleave(o.rounds, o.warmup, &mut base_fn, &mut cand_fn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn set_with(name: &str, v: f64) -> Set {
        let mut s = Set::new();
        s.record(name, v);
        s
    }

    /// Records the order the two sides actually ran in.
    fn recording(
        label: &'static str,
        log: Rc<RefCell<Vec<&'static str>>>,
        value: f64,
    ) -> impl FnMut(usize) -> Result<Set, String> {
        move |_round| {
            log.borrow_mut().push(label);
            Ok(set_with("b", value))
        }
    }

    #[test]
    fn interleave_collects_one_observation_per_side_per_round() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut base = recording("base", log.clone(), 100.0);
        let mut cand = recording("cand", log.clone(), 50.0);
        let (b, c) = interleave(4, false, &mut base, &mut cand).unwrap();
        assert_eq!(b.values("b").unwrap().len(), 4);
        assert_eq!(c.values("b").unwrap().len(), 4);
    }

    // Alternating rounds cancels drift BETWEEN rounds. Running the sides in a
    // fixed order WITHIN each round leaves a systematic offset: the candidate
    // would always be measured one slot later, so any drift monotonic across a
    // round lands on it in the same direction every time. That is a constant
    // bias, not noise, and averaging does not remove it.
    #[test]
    fn the_two_sides_swap_order_every_round() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut base = recording("base", log.clone(), 100.0);
        let mut cand = recording("cand", log.clone(), 50.0);
        interleave(4, false, &mut base, &mut cand).unwrap();
        assert_eq!(
            *log.borrow(),
            vec![
                "base", "cand", "cand", "base", "base", "cand", "cand", "base"
            ]
        );
    }

    // However the sides ran, the results must come back in (base, cand) order.
    #[test]
    fn results_are_returned_in_base_cand_order_regardless_of_run_order() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut base = recording("base", log.clone(), 100.0);
        let mut cand = recording("cand", log.clone(), 50.0);
        let (b, c) = interleave(2, false, &mut base, &mut cand).unwrap();
        assert!(b.values("b").unwrap().iter().all(|&v| v == 100.0));
        assert!(c.values("b").unwrap().iter().all(|&v| v == 50.0));
    }

    #[test]
    fn the_warmup_round_is_run_and_discarded() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut base = recording("base", log.clone(), 100.0);
        let mut cand = recording("cand", log.clone(), 50.0);
        let (b, _) = interleave(3, true, &mut base, &mut cand).unwrap();
        assert_eq!(log.borrow().len(), 8, "4 rounds run: 1 warmup + 3 measured");
        assert_eq!(b.values("b").unwrap().len(), 3, "only 3 kept");
    }

    #[test]
    fn fewer_than_two_rounds_is_an_error() {
        let mut base = |_: usize| Ok(set_with("b", 1.0));
        let mut cand = |_: usize| Ok(set_with("b", 1.0));
        assert!(interleave(1, false, &mut base, &mut cand).is_err());
    }

    #[test]
    fn an_error_from_either_side_names_the_side_and_the_round() {
        let mut base = |_: usize| Ok(set_with("b", 1.0));
        let mut cand = |_: usize| Err("boom".to_string());
        let err = interleave(2, false, &mut base, &mut cand).unwrap_err();
        assert!(err.contains("candidate"), "{err}");
        assert!(err.contains("round"), "{err}");
    }

    #[test]
    fn the_filter_anchors_and_escapes_every_name() {
        assert_eq!(bench_filter(&["a".into(), "b".into()]), "^(a|b)$");
        // A hand-edited config name with a metacharacter must not silently
        // BROADEN the pattern to match benchmarks nobody selected.
        assert_eq!(bench_filter(&["a.b".into()]), r"^(a\.b)$");
        assert_eq!(bench_filter(&["p/s".into()]), "^(p/s)$");
        assert_eq!(bench_filter(&[]), ".");
    }
}
