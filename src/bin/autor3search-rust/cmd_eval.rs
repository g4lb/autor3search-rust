//! The `eval` command: flags, output formatting and the exit code only.
//!
//! Everything that decides KEEP/DISCARD/FAIL/CRASH lives in
//! [`autor3search::pipeline`], testable without a process boundary. This file
//! is deliberately thin — it resolves the run from the current branch, opens
//! the run log, calls the pipeline once, records a `results.tsv` row and
//! prints one of two output shapes.

use crate::args::Args;
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
    match eval(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            2
        }
    }
}

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
    // The row was just appended, so its own count is this experiment's
    // ordinal position in the run.
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
