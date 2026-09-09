//! Runs one full evaluation: gates correctness, measures a candidate against
//! the pinned baseline, and returns a verdict.
//!
//! It lives in the library rather than the binary so it is testable without a
//! process boundary — the eval command handles only flags, output formatting
//! and the exit code.

use crate::bench;
use crate::config::Config;
use crate::discover;
use crate::freeze::{self, FreezeError};
use crate::gitx;
use crate::measure;
use crate::results;
use crate::scope;
use crate::state::{self, Baseline};
use crate::verdict::{self, Reason, Status, VerdictResult};
use std::io::Write;
use std::path::PathBuf;

/// The harness-owned scratch log inside the repository root. Subprocess output
/// that could flood an unattended agent's context goes here rather than to
/// stdout. Gitignored by `init`, and not part of the score.
pub const RUN_LOG_NAME: &str = "run.log";

pub struct Options {
    pub root: PathBuf,
    pub state_dir: PathBuf,
    pub cfg: Config,
    /// `measure_commit` is UPDATED and persisted as a side effect of a KEEP.
    /// `commit`, the frozen anchor, is only ever read.
    pub baseline: Baseline,
}

/// Every unit compared for one experiment. Time is the only scored metric —
/// there is no allocation metric to report, since criterion measures time
/// alone.
pub struct Measurements {
    pub time: Vec<bench::Delta>,
}

/// Reborrows `log` for one subprocess call rather than moving it.
///
/// `eval` calls into several subprocesses in turn, each wanting its own
/// `Option<&mut dyn Write>`. Calling `Option::as_deref_mut()` directly at
/// each site ties the returned borrow's lifetime to the generic `DerefMut`
/// impl's own parameter rather than to the individual call, which the borrow
/// checker then refuses to let end before `eval` itself returns — so the
/// very next call in sequence is rejected as a second, overlapping mutable
/// borrow. A plain function with a single named lifetime does not have that
/// problem. `crate::measure::side` documents the same issue for the same
/// reason, there against a `RefCell` rather than a bare `Option`.
fn reborrow_log<'a>(log: &'a mut Option<&mut dyn Write>) -> Option<&'a mut dyn Write> {
    match log {
        Some(w) => Some(&mut **w),
        None => None,
    }
}

/// The result for an experiment `stop --force` cancelled before it could
/// finish. Not a verdict — `program.md` documents `ABORTED` as an
/// interrupted experiment, distinct from the four real outcomes — and
/// `eval` (the command) never writes a `results.tsv` row for it: nothing
/// was measured, so nothing is recorded.
fn aborted() -> VerdictResult {
    verdict::gate(
        Status::Aborted,
        Reason::StopForced,
        "cancelled by 'stop --force' before this experiment finished",
    )
}

/// Gates, measures and scores one experiment.
///
/// Returns a terminal verdict for every gate outcome and every completed
/// measurement. An `Err` means the harness itself malfunctioned — I/O, git, a
/// malformed baseline — not that the candidate was rejected.
pub fn eval(
    o: &mut Options,
    mut log: Option<&mut dyn Write>,
) -> Result<(VerdictResult, Option<Measurements>), String> {
    // Checked before gate 1: a `stop --force` that arrives while `eval` is
    // still doing setup (resolving the run, loading the baseline — none of
    // which spawns a subprocess for `Runner` to catch it in) must not run
    // any of the gates below at all.
    if state::CANCEL_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok((aborted(), None));
    }

    let timeout = o.cfg.timeout_duration()?;

    // 1. Scope, and 2. locked files. Checked before anything is restored or
    //    built, so an out-of-scope edit is reported as itself rather than as a
    //    build error.
    //
    //    Diffs against baseline.commit — the FROZEN anchor — and NOT
    //    measure_commit, which moves after every KEEP. A fixed anchor means the
    //    gate keeps re-validating the FULL accumulated diff on every eval.
    //    Anchored to the advancing pointer instead, an out-of-scope edit would
    //    get exactly one eval in which to be caught before becoming part of the
    //    "already accepted" state and permanently invisible.
    let changed = gitx::changed_since(&o.root, &o.baseline.commit)?;
    let matcher = scope::Matcher::new(&o.cfg.scope);
    for rel in &changed {
        // A symlink — the changed path itself, or any component beneath the
        // repository root on the way to it — defeats both checks below by
        // construction: `git status` reports the link (e.g. `.cargo`, or a
        // workspace-member directory) as one atomic entry, so neither
        // `locked_file`'s basename match nor the scope `Matcher`'s glob ever
        // see the real, locked path (`.cargo/config.toml`) that the link
        // actually resolves to when cargo — which does follow symlinks —
        // reads it at build time. The harness cannot reason about what is
        // behind a link it hasn't been told to trust, so it declines rather
        // than guessing. `freeze::symlink_component` is the same check
        // `freeze.rs` already applies to frozen test files, including the
        // deliberate exception for the repository root itself (a repo
        // legitimately reached through a symlinked ancestor, e.g. macOS's
        // `/tmp`, is not tampering).
        if let Some(link) = freeze::symlink_component(&o.root, rel).map_err(|e| e.to_string())? {
            return Ok((
                verdict::gate(
                    Status::Fail,
                    Reason::ScopeViolation,
                    format!(
                        "{rel}: {link} is a symlink; refusing to match a path reached through a \
                         symlink against locked files or scope, because the link can make a \
                         locked file (or a path outside the repository) look like an ordinary, \
                         differently-named change. Replace {link} with a real file or directory \
                         and rerun."
                    ),
                ),
                None,
            ));
        }
        if let Some(reason) = scope::locked_file(rel) {
            return Ok((
                verdict::gate(
                    Status::Fail,
                    Reason::ScopeViolation,
                    format!("{rel} may not be modified: {reason}"),
                ),
                None,
            ));
        }
        if rel == results::RESULTS_PATH || rel == RUN_LOG_NAME || rel == crate::config::CONFIG_PATH
        {
            continue; // harness output, plus the config, integrity-checked below
        }
        if rel.starts_with("tests/") || rel.starts_with("benches/") {
            continue; // handled by restore, not by the scope gate
        }
        if !matcher.matches(rel) {
            return Ok((
                verdict::gate(
                    Status::Fail,
                    Reason::ScopeViolation,
                    format!("{rel} is outside the allowed scope {:?}", o.cfg.scope),
                ),
                None,
            ));
        }
    }

    // 2b. Locked files, independent of git. The loop above only ever sees
    //     what `git diff`/`git status` report, and both omit gitignored
    //     paths — so a repository (or config) that gitignores `Cargo.lock`,
    //     or an agent that edits an in-scope `.gitignore` to add `.cargo/`
    //     before creating `.cargo/config.toml`, would sail through it
    //     untouched. This stats every locked path directly instead of
    //     trusting git's view, so it catches exactly that. Defence in depth,
    //     not a replacement for the check above.
    let current_locked: std::collections::BTreeMap<String, String> =
        discover::locked_files(&o.root)?
            .into_iter()
            .map(|rel| {
                let hash = freeze::sha256_file(&o.root.join(&rel))?;
                Ok::<_, String>((rel, hash))
            })
            .collect::<Result<_, String>>()?;
    if current_locked != o.baseline.locked_files {
        let mut details: Vec<String> = Vec::new();
        for (rel, hash) in &current_locked {
            match o.baseline.locked_files.get(rel) {
                None => details.push(format!(
                    "{rel} added: {}",
                    scope::locked_file(rel).unwrap_or("locked at any depth")
                )),
                Some(h) if h != hash => details.push(format!(
                    "{rel} modified: {}",
                    scope::locked_file(rel).unwrap_or("locked at any depth")
                )),
                _ => {}
            }
        }
        for rel in o.baseline.locked_files.keys() {
            if !current_locked.contains_key(rel) {
                details.push(format!(
                    "{rel} removed: {}",
                    scope::locked_file(rel).unwrap_or("locked at any depth")
                ));
            }
        }
        return Ok((
            verdict::gate(
                Status::Fail,
                Reason::ScopeViolation,
                format!(
                    "locked file(s) changed since baseline, independent of what git reports as \
                     changed (this catches a gitignored locked path): {}",
                    details.join("; ")
                ),
            ),
            None,
        ));
    }

    // 3. Config integrity. config.yaml lives in the repo because humans own
    //    it, which means the agent can reach it; raising max_regress_pct or
    //    dropping a benchmark would defeat the guard.
    let cfg_sum = freeze::sha256_file(&o.root.join(crate::config::CONFIG_PATH))?;
    if cfg_sum != o.baseline.config_sha256 {
        return Ok((
            verdict::gate(
                Status::Fail,
                Reason::ConfigChanged,
                format!(
                    "{} changed since baseline — scoring rules are fixed for a run; revert it, \
                     or start a new run with 'autor3search-rust baseline --tag <tag>'",
                    crate::config::CONFIG_PATH
                ),
            ),
            None,
        ));
    }

    // 4. Restore the frozen tests that live in their own files. Agent edits
    //    are erased, not argued with.
    let manifest = freeze::Manifest::load(&o.state_dir.join(freeze::MANIFEST_PATH))?;
    let store = o.state_dir.join(freeze::STORE_DIR);
    match freeze::restore(&o.root, &store, &manifest) {
        Ok(restored) => {
            if !restored.is_empty() {
                if let Some(log) = reborrow_log(&mut log) {
                    let _ = writeln!(
                        log,
                        "restored {} frozen file(s): {restored:?}",
                        restored.len()
                    );
                }
            }
        }
        // A frozen file replaced by a symlink is tampering, not a harness
        // malfunction: it earns a results.tsv row and an actionable message
        // rather than aborting with no signal at all.
        Err(FreezeError::Symlink(msg)) => {
            return Ok((
                verdict::gate(
                    Status::Fail,
                    Reason::SymlinkSwap,
                    format!(
                        "{msg} — a frozen test file, and every directory on the way to it, must \
                         remain a regular file and real directories; restore them and rerun"
                    ),
                ),
                None,
            ));
        }
        // Likewise, but unrecoverable: the reference copy is the thing that
        // was lost, so fixing the working tree cannot undo it.
        Err(FreezeError::StoreTampered(msg)) => {
            return Ok((
                verdict::gate(
                    Status::Fail,
                    Reason::FrozenStoreTampered,
                    format!(
                        "{msg} — the frozen copy this run scores against was modified, so its \
                         tests can no longer be trusted. Start a fresh run with \
                         'autor3search-rust baseline --tag <tag>'."
                    ),
                ),
                None,
            ));
        }
        Err(FreezeError::Io(e)) => return Err(e),
    }

    // 5. Inline tests (Rust-specific). These live inside files the agent is
    //    entitled to edit, so they cannot be restored without erasing the
    //    optimization too — they are hashed at baseline and any change is
    //    refused instead.
    let changed_inline = freeze::verify_inline(&o.root, &manifest).map_err(|e| e.to_string())?;
    if !changed_inline.is_empty() {
        return Ok((
            verdict::gate(
                Status::Fail,
                Reason::InlineTestModified,
                format!(
                    "inline tests changed in {changed_inline:?} — a #[cfg(test)] module or a \
                     doctest inside a source file you may otherwise edit. These cannot be \
                     restored for you, because restoring the file would erase your \
                     optimization with them. Revert the test to its baseline form and rerun."
                ),
            ),
            None,
        ));
    }

    // 6. The frozen set and what is on disk must agree in BOTH directions.
    //
    //    Restore only rewrites files it froze, and the scope gate skips
    //    tests/ and benches/, so without the first check an agent could ADD a
    //    brand-new test file — an easier benchmark, or one shadowing a frozen
    //    one — and no gate would notice.
    //
    //    The reverse direction is easier to miss and matters as much: a file
    //    restore just rewrote should be visible to the walker again, so a
    //    manifest entry MISSING from the walk means the walk cannot reach it.
    //    A directory renamed or moved under target/ or a dot-prefixed name
    //    hides a frozen test from discovery while leaving it nominally
    //    restored, and the run would keep scoring against a benchmark set
    //    that no longer runs.
    let present = discover::frozen_test_files(&o.root, &o.cfg.unfreeze)?;
    let added: Vec<&String> = present
        .iter()
        .filter(|p| !manifest.files.contains_key(*p))
        .collect();
    if !added.is_empty() {
        return Ok((
            verdict::gate(
                Status::Fail,
                Reason::NewTestFile,
                format!(
                    "test files not present at baseline: {added:?} — the benchmark set is \
                     frozen; add them before running 'autor3search-rust baseline', or list \
                     them in the config's unfreeze"
                ),
            ),
            None,
        ));
    }
    let missing: Vec<&String> = manifest
        .files
        .keys()
        .filter(|k| !present.contains(k))
        .collect();
    if !missing.is_empty() {
        return Ok((
            verdict::gate(
                Status::Fail,
                Reason::MissingTestFile,
                format!(
                    "frozen test files are no longer discoverable in the working tree: \
                     {missing:?} — they were restored, but the walk that finds test files \
                     cannot reach them, so they would not run. Check for a directory on their \
                     path that was replaced, renamed, or moved under target/ or a \
                     dot-prefixed name."
                ),
            ),
            None,
        ));
    }

    let runner = crate::runner::Runner::new(&o.root, timeout);

    // 7. Build, in release: that is what the benchmarks are compiled with.
    let out = runner.cargo(
        &["build", "--release", "--all-targets", "--workspace"],
        reborrow_log(&mut log),
    )?;
    // Checked before `timed_out`/`ok()`: a build killed by `stop --force`
    // exits non-zero exactly like a real build failure would, and reporting
    // it as BuildFailed would be actively misleading about what happened.
    if out.cancelled {
        return Ok((aborted(), None));
    }
    if out.timed_out {
        return Ok((
            verdict::gate(Status::Crash, Reason::Timeout, "cargo build timed out"),
            None,
        ));
    }
    if !out.ok() {
        return Ok((
            verdict::gate(Status::Crash, Reason::BuildFailed, out.tail(30)),
            None,
        ));
    }

    // 8. Correctness, in DEBUG. Speed is never traded for correctness, and
    //    debug is the stricter place to check it: debug_assert! fires and
    //    arithmetic overflow panics rather than wrapping, which catches
    //    exactly the class of bug an optimization pass introduces. This is the
    //    nearest thing Rust has to the extra checking `go vet` and `-race`
    //    bought the Go original.
    let out = runner.cargo(&["test", "--workspace"], reborrow_log(&mut log))?;
    if out.cancelled {
        return Ok((aborted(), None));
    }
    if out.timed_out {
        return Ok((
            verdict::gate(Status::Crash, Reason::Timeout, "cargo test timed out"),
            None,
        ));
    }
    if !out.ok() {
        return Ok((
            verdict::gate(Status::Fail, Reason::TestsFailed, out.tail(40)),
            None,
        ));
    }

    // 9. Baseline worktree integrity. An agent could edit the pinned worktree
    //    to make the BASELINE slow, after which every candidate "improves" and
    //    every experiment returns KEEP without optimizing anything.
    //
    //    Compares against measure_commit, not commit: the worktree is
    //    re-pointed after every KEEP, so comparing against the frozen anchor
    //    would fire spuriously on the very next eval after any KEEP.
    //
    //    This is DETECTION and only partial: the state directory is outside
    //    the repo, but the agent runs as the same OS user, so nothing stops it
    //    editing the worktree in place. It is caught only if HEAD moves. Treat
    //    it as catching accidental clobbering and a careless tamper, not as a
    //    guarantee.
    let worktree = o.state_dir.join(state::WORKTREE_NAME);
    let worktree_head = gitx::head_commit(&worktree)?;
    if worktree_head != o.baseline.measure_commit {
        return Ok((
            verdict::gate(
                Status::Fail,
                Reason::BaselineTampered,
                format!(
                    "pinned baseline worktree HEAD is {worktree_head} but the recorded \
                     measurement commit is {} — the worktree no longer matches the baseline \
                     and this run's measurements cannot be trusted. Start a fresh run with \
                     'autor3search-rust baseline --tag <tag>'.",
                    o.baseline.measure_commit
                ),
            ),
            None,
        ));
    }

    // 10. Measure, interleaved against the pinned worktree.
    let opts = measure::Options {
        base_dir: worktree,
        cand_dir: o.root.clone(),
        targets: o.baseline.bench_targets.clone(),
        benchmarks: o.baseline.benchmarks.clone(),
        sample_size: o.cfg.sample_size,
        measurement_secs: o.cfg.measurement_secs()?,
        warm_up_secs: o.cfg.warm_up_secs()?,
        rounds: o.cfg.count,
        warmup: true,
        timeout,
        criterion_root: o.state_dir.join(state::CRITERION_DIR),
    };
    let (base_set, cand_set) = match measure::run(&opts, reborrow_log(&mut log)) {
        Ok(sets) => sets,
        // `stop --force` mid-measurement: nothing was fully measured, so
        // this is an abandoned experiment, not a broken one — checked
        // before the generic error case below, which would otherwise
        // report it as a build failure.
        Err(e) if e == measure::CANCELLED_SENTINEL => return Ok((aborted(), None)),
        // A benchmark that will not run is a broken candidate, not a broken
        // harness: report it as a verdict so the loop records a row and
        // carries on rather than stopping the run.
        Err(e) => return Ok((verdict::gate(Status::Crash, Reason::BuildFailed, e), None)),
    };

    // 11-12. Score.
    let deltas = bench::compare_all(&base_set, &cand_set)?;
    let score = bench::geomean(&deltas)?;

    // 13. Decide.
    let result = verdict::decide(verdict::Input {
        deltas: deltas.clone(),
        score,
        max_regress_pct: o.cfg.max_regress_pct,
        min_effect_pct: o.cfg.min_effect_pct,
    });

    // 14. Advance the measurement baseline on KEEP. Without this, every
    //     experiment after the first kept one is measured against the run's
    //     ORIGINAL commit forever, so a later no-op that merely fails to
    //     regress an EARLIER improvement still banks as KEEP.
    if result.status == Status::Keep {
        advance_measurement_baseline(o)?;
    }

    Ok((result, Some(Measurements { time: deltas })))
}

/// Re-points the pinned worktree at the candidate's own commit and persists it
/// as the new `measure_commit`, after a KEEP.
///
/// Without this, every experiment after the first kept one is measured against
/// the run's ORIGINAL commit forever, so a later no-op that merely fails to
/// regress an EARLIER improvement still banks as KEEP.
///
/// Moves the worktree FIRST, then persists — and rolls the checkout back to
/// `old_commit` if the save fails, rather than persisting first. A failed
/// `save` (disk full — this project has hit that twice during its own
/// development) is a harness malfunction, not tampering, and it must not be
/// allowed to look like one: left un-rolled-back, the worktree would sit at
/// `new_commit` while `baseline.json` on disk still names `old_commit` (this
/// function never wrote it), and gate 9 on the very next `eval` would see
/// that mismatch and report `FAIL(baseline_tampered)` — blaming tampering
/// for a failed write and bricking an otherwise-fine overnight run. Rolling
/// the checkout back instead restores the one invariant gate 9 checks
/// (worktree `HEAD` == the `measure_commit` actually on disk) before this
/// function returns its error.
fn advance_measurement_baseline(o: &mut Options) -> Result<(), String> {
    let old_commit = o.baseline.measure_commit.clone();
    let new_commit = gitx::head_commit(&o.root)?;
    let worktree = o.state_dir.join(state::WORKTREE_NAME);

    gitx::checkout_detached(&worktree, &new_commit)?;
    o.baseline.measure_commit = new_commit.clone();
    if let Err(save_err) = o.baseline.save(&o.state_dir.join(state::BASELINE_FILE)) {
        o.baseline.measure_commit = old_commit.clone();
        if let Err(rollback_err) = gitx::checkout_detached(&worktree, &old_commit) {
            return Err(format!(
                "{save_err}\n\nadditionally, failed to roll the pinned worktree back to \
                 {old_commit} after that failure: {rollback_err} — it now points at \
                 {new_commit} while baseline.json on disk still names {old_commit}, which the \
                 next eval will misreport as FAIL(baseline_tampered). Fix by hand: cd {} && git \
                 checkout -q -f --detach {old_commit}",
                worktree.display()
            ));
        }
        return Err(save_err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // I4: a `save` that fails AFTER the worktree has already moved must not
    // leave gate 9's invariant broken (pinned worktree HEAD == the
    // measure_commit actually recorded on disk) — otherwise the very next
    // eval blames tampering for what was really a failed write. Simulated
    // portably, with no platform-specific permission bits: a directory
    // sitting where `baseline.json` needs to be written makes `save` fail
    // deterministically on every platform.
    #[test]
    fn a_failed_save_rolls_the_worktree_checkout_back() {
        let repo_dir = tempfile::tempdir().unwrap();
        let root = repo_dir.path().canonicalize().unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "first"]);
        let first = gitx::head_commit(&root).unwrap();

        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "second"]);

        let state_dir = tempfile::tempdir().unwrap();
        let worktree = state_dir.path().join(state::WORKTREE_NAME);
        gitx::add_worktree(&root, &worktree, &first).unwrap();

        // Force the save to fail: a directory where the file must go.
        std::fs::create_dir_all(state_dir.path().join(state::BASELINE_FILE)).unwrap();

        let mut o = Options {
            root: root.clone(),
            state_dir: state_dir.path().to_path_buf(),
            cfg: Config::default(),
            baseline: Baseline {
                tag: "t".into(),
                branch: "autor3search-rust/t".into(),
                commit: first.clone(),
                measure_commit: first.clone(),
                created_at: String::new(),
                benchmarks: Vec::new(),
                bench_targets: Vec::new(),
                config_sha256: "0".repeat(64),
                locked_files: std::collections::BTreeMap::new(),
            },
        };

        let err = advance_measurement_baseline(&mut o).unwrap_err();
        assert!(err.contains("baseline"), "{err}");

        // The worktree must be back at the OLD commit, and the in-memory
        // record must agree with what is still on disk.
        assert_eq!(gitx::head_commit(&worktree).unwrap(), first);
        assert_eq!(o.baseline.measure_commit, first);
    }
}
