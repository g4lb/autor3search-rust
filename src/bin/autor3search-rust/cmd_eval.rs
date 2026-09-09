//! The `eval` command: flags, output formatting and the exit code only.
//!
//! Everything that decides KEEP/DISCARD/FAIL/CRASH lives in
//! [`autor3search::pipeline`], testable without a process boundary. This file
//! is deliberately thin — it resolves the run from the current branch, opens
//! the run log, calls the pipeline once, records a `results.tsv` row and
//! prints one of two output shapes.
//!
//! It also claims the run for the lifetime of the process (so `stop --force`
//! has a real, verified-live pid to signal rather than a guess) and installs
//! a `SIGTERM` handler that asks the pipeline to abort at its next safe
//! checkpoint — see [`install_cancel_handler`] and
//! `autor3search::state::CANCEL_REQUESTED`.

use crate::args::Args;
use autor3search::verdict::Status;
use autor3search::{config, gitx, pipeline, results, state};
use serde_json::json;
use std::io::Write;

pub fn run(argv: &[String]) -> i32 {
    let args = match Args::parse(
        argv,
        &[
            ("C", true),
            ("json", false),
            ("desc", true),
            ("no-log", false),
        ],
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return autor3search::EXIT_USAGE;
        }
    };
    install_cancel_handler();
    match eval(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            2
        }
    }
}

/// A pid this process is asked to signal via `libc::signal` merely stores a
/// function pointer, so the handler itself can only touch static memory —
/// see `autor3search::state::CANCEL_REQUESTED`, which is exactly that: a
/// process-wide flag, set here and polled from inside `Runner::cargo`'s own
/// wait loop and at a few points in the pipeline between gates, so a signal
/// arriving mid-subprocess is noticed within one poll tick rather than only
/// once the whole subprocess finishes.
#[cfg(unix)]
extern "C" fn on_sigterm(_sig: libc::c_int) {
    state::CANCEL_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(unix)]
fn install_cancel_handler() {
    // SAFETY: `on_sigterm` only stores to an `AtomicBool`, which is
    // async-signal-safe; it touches no other shared state and allocates
    // nothing.
    unsafe {
        if libc::signal(libc::SIGTERM, on_sigterm as *const () as libc::sighandler_t)
            == libc::SIG_ERR
        {
            eprintln!(
                "warning: could not install a SIGTERM handler — 'stop --force' will only be \
                 able to hard-kill this process rather than ask it to finish gracefully"
            );
        }
    }
}

/// Windows has no `SIGTERM` this process could catch mid-benchmark, so
/// `stop --force` there terminates outright instead — see `cmd_stop`'s
/// platform-specific `terminate`. There is nothing to install here.
#[cfg(not(unix))]
fn install_cancel_handler() {}

fn eval(args: &Args) -> Result<i32, String> {
    let root = gitx::root(&args.dir())?;

    // The run is identified by the branch `baseline` created, never by a flag:
    // an agent working the loop just runs `eval` from wherever `baseline` left
    // it checked out.
    let branch = gitx::current_branch(&root)?;
    let tag = state::tag_from_branch(&branch).ok_or_else(|| {
        format!(
            "current branch {branch:?} was not created by 'autor3search-rust baseline --tag \
             <tag>' — check out a run branch first"
        )
    })?;
    let dir = state::state_dir(&root, &tag)?;

    // Claimed for the rest of this function's lifetime: `_claim`'s `Drop`
    // releases it on every return path from here on, including an early `?`
    // — see `state::claim_eval`. Claimed before anything else so `stop
    // --force`, run at any moment from now on, sees a real, live pid rather
    // than nothing.
    let _claim = state::claim_eval(&dir)?;

    let cfg = config::Config::load(&root.join(config::CONFIG_PATH))?;
    let baseline = state::Baseline::load(&dir.join(state::BASELINE_FILE))?;

    let mut log_file = if args.flag("no-log") {
        None
    } else {
        Some(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(root.join(pipeline::RUN_LOG_NAME))
                .map_err(|e| format!("open {}: {e}", pipeline::RUN_LOG_NAME))?,
        )
    };
    let log: Option<&mut dyn Write> = log_file.as_mut().map(|f| f as &mut dyn Write);

    let root_commit = gitx::head_commit(&root)?;
    let worktree = dir.join(state::WORKTREE_NAME);

    let mut o = pipeline::Options {
        root: root.clone(),
        state_dir: dir.clone(),
        cfg,
        baseline,
    };
    let (result, measurements) = pipeline::eval(&mut o, log)?;

    // Informational only, and never gates this experiment's own verdict:
    // `stop` (Task 19) asks the agent to end the loop AFTER the experiment
    // already under way, not to abandon it mid-flight.
    let stop_requested = state::stop_requested(&dir);

    // The largest single-benchmark improvement, i.e. the most negative
    // pct_change — absent for a gate failure, which never reaches measurement.
    let best_bench_delta = measurements
        .as_ref()
        .and_then(|m| m.time.iter().map(|d| d.pct_change).reduce(f64::min))
        .unwrap_or(0.0);

    let status_str = enum_str(&result.status);

    let results_path = root.join(results::RESULTS_PATH);
    // ABORTED is not a verdict: `stop --force` cancelled the experiment
    // before it finished, so nothing was measured and nothing is recorded —
    // program.md promises exactly this, and a row for an experiment that
    // never ran would misreport the run's actual progress.
    if result.status != Status::Aborted {
        results::append(
            &results_path,
            &results::Row {
                commit: root_commit,
                score: result.score,
                best_bench_delta,
                status: status_str.clone(),
                description: args.value("desc").unwrap_or("").to_string(),
            },
        )?;
    }
    // The row was just appended (unless aborted), so its own count is this
    // experiment's ordinal position in the run.
    let experiment = results::load(&results_path)?.len();

    let run_obj = json!({
        "tag": o.baseline.tag,
        "branch": branch,
        "baseline_commit": o.baseline.commit,
        "measure_commit": o.baseline.measure_commit,
        "worktree": worktree.display().to_string(),
        "experiment": experiment,
    });

    if args.flag("json") {
        // Exactly one JSON object on stdout: program.md parses it, so nothing
        // else may share this stream.
        let mut value =
            serde_json::to_value(&result).map_err(|e| format!("encode verdict: {e}"))?;
        if let serde_json::Value::Object(map) = &mut value {
            map.insert("stop_requested".to_string(), json!(stop_requested));
            map.insert("run".to_string(), run_obj);
        }
        println!("{value}");
    } else {
        for w in &result.warnings {
            println!("WARNING: {w}");
        }
        println!(
            "VERDICT: {status_str} ({}) score={:.4}",
            enum_str(&result.reason),
            result.score
        );
        if !result.message.is_empty() {
            println!("{}", result.message);
        }
        for d in &result.regressions {
            println!("  regression: {} {:+.1}%", d.name, d.pct_change);
        }
        if stop_requested {
            println!("NOTE: a stop has been requested — end the loop after this experiment");
        }
    }

    Ok(result.exit_code())
}

/// The exact string a `Status` or `Reason` serializes to (`"KEEP"`,
/// `"scope_violation"`, ...), so the human-readable block never drifts from
/// the JSON one by re-deriving a second spelling from `Debug`.
fn enum_str<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}
