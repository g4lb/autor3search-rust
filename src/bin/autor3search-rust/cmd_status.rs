//! The `status` command: shows where a run is without touching anything.
//!
//! It exists because `eval`'s own output cannot answer "where is this run,
//! and how do I stop it": `eval --json`'s contract is one JSON object and
//! nothing else, and the human watching may not even be reading the agent's
//! transcript. This is the command they run in their own shell, at any
//! moment, from any branch.
//!
//! **It opens nothing for writing.** Checking on a run must never be able to
//! change it — see `tests/cmd_status_stop.rs`, which snapshots the tree
//! either side of the call to prove it. It also prints plain text only:
//! `program.md` promises there is no `--json` form and no `stop_requested`
//! field of its own, because the machine-readable stop signal already rides
//! on `eval --json`.

use crate::args::Args;
use autor3search::{gitx, results, state};
use std::path::Path;

pub fn run(argv: &[String]) -> i32 {
    let args = match Args::parse(argv, &[("C", true), ("tag", true)]) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return autor3search::EXIT_USAGE;
        }
    };
    match status(&args) {
        Ok(msg) => {
            print!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            2
        }
    }
}

/// Resolves which run to describe: an explicit `--tag`, or — when the
/// current branch is itself a run branch — the tag it encodes. Refuses to
/// guess from anywhere else: a human standing on an unrelated branch with no
/// `--tag` has given this command nothing to infer from.
fn resolve_tag(root: &Path, tag_flag: Option<&str>) -> Result<String, String> {
    if let Some(t) = tag_flag {
        state::valid_tag(t)?;
        return Ok(t.to_string());
    }
    let branch = gitx::current_branch(root)?;
    state::tag_from_branch(&branch).ok_or_else(|| {
        format!(
            "current branch {branch:?} was not created by 'autor3search-rust baseline --tag \
             <tag>' — pass --tag <tag>, or check out the run branch first"
        )
    })
}

fn status(args: &Args) -> Result<String, String> {
    let root = gitx::root(&args.dir())?;
    let tag = resolve_tag(&root, args.value("tag"))?;
    let dir = state::state_dir(&root, &tag)?;
    let baseline = state::Baseline::load(&dir.join(state::BASELINE_FILE))?;
    let run_branch = state::branch_name(&tag);
    let current_branch = gitx::current_branch(&root)?;

    let mut out = String::new();
    out.push_str(&format!("{:<14} {tag}\n", "run tag"));
    if current_branch == run_branch {
        out.push_str(&format!("{:<14} {run_branch}  (checked out)\n", "branch"));
    } else {
        out.push_str(&format!(
            "{:<14} {run_branch}  (NOT checked out — you are on {current_branch})\n",
            "branch"
        ));
    }
    out.push_str(&format!(
        "{:<14} {}  (run started here)\n",
        "baseline", baseline.commit
    ));
    if baseline.measure_commit == baseline.commit {
        out.push_str(&format!(
            "{:<14} {}  (still the baseline — nothing kept yet)\n",
            "measuring vs", baseline.measure_commit
        ));
    } else {
        out.push_str(&format!(
            "{:<14} {}  (advanced past the baseline by earlier KEEPs)\n",
            "measuring vs", baseline.measure_commit
        ));
    }
    let worktree = dir.join(state::WORKTREE_NAME);
    if worktree.is_dir() {
        out.push_str(&format!("{:<14} {}\n", "worktree", worktree.display()));
    } else {
        // `baseline --tag {tag} --force` cannot fix this: its tag-collision
        // check is unconditional (see `cmd_baseline.rs`) and refuses to run
        // again for a tag that already has a baseline, --force or not —
        // --force only ever gates the separate results.tsv check. The
        // worktree is the only thing missing here (baseline.json, just
        // loaded above, is fine), so the real fix is recreating just it, by
        // hand, at the commit this run is actually measuring against.
        out.push_str(&format!(
            "{:<14} {}  (MISSING — recreate it by hand:\n{:<16}git -C {} worktree add \
             --detach -f {} {}\n{:<16}'baseline --tag {tag} --force' will NOT do this: a tag \
             that already has a baseline is always refused, --force or not)\n",
            "worktree",
            worktree.display(),
            "",
            root.display(),
            worktree.display(),
            baseline.measure_commit,
            "",
        ));
    }
    out.push_str(&experiments_line(&root));
    out.push_str(&stop_lines(&dir));
    Ok(out)
}

/// "How far into the loop is it", from `results.tsv` — the same file
/// `report` reads. A missing or unreadable file is reported as such rather
/// than as zero experiments: silently claiming a run has done nothing would
/// be worse than admitting the count is unavailable.
fn experiments_line(root: &Path) -> String {
    let rows = match results::load(&root.join(results::RESULTS_PATH)) {
        Ok(r) => r,
        Err(e) => return format!("{:<14} unavailable ({e})\n", "experiments"),
    };
    if rows.is_empty() {
        return format!("{:<14} none yet\n", "experiments");
    }
    let mut keep = 0;
    let mut discard = 0;
    let mut fail = 0;
    let mut crash = 0;
    for r in &rows {
        match r.status.as_str() {
            "KEEP" => keep += 1,
            "DISCARD" => discard += 1,
            "FAIL" => fail += 1,
            "CRASH" => crash += 1,
            _ => {}
        }
    }
    format!(
        "{:<14} {} run  ({keep} keep, {discard} discard, {fail} fail, {crash} crash)  — next is \
         #{}\n",
        "experiments",
        rows.len(),
        rows.len() + 1
    )
}

fn stop_lines(dir: &Path) -> String {
    if state::stop_requested(dir) {
        format!(
            "{:<14} requested — the agent will exit the loop at its next verdict\n\nto cancel \
             the stop:  autor3search-rust stop --clear\nto stop sooner:      autor3search-rust \
             stop --force\n",
            "stop"
        )
    } else {
        format!(
            "{:<14} not requested\n\nto stop after the current experiment:  autor3search-rust \
             stop\nto stop now, abandoning it:            autor3search-rust stop --force\n",
            "stop"
        )
    }
}
