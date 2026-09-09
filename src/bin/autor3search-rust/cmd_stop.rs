//! The `stop` command: asks a run to end.
//!
//! Two speeds, and the difference is who decides when to stop:
//!
//! - Plain `stop` writes a request the AGENT reads at its next verdict. The
//!   experiment under way finishes and is scored, its KEEP or DISCARD is
//!   applied, and only then does the loop exit. Nothing is thrown away.
//! - `stop --force` additionally signals the running `eval` to abandon the
//!   experiment now, for when the human cannot wait for a long benchmark to
//!   finish. It reports what state that leaves the repository in and drops
//!   nothing itself — dropping an in-flight commit is the human's call.
//!
//! Both leave the repository on the run branch with every kept commit
//! intact.
//!
//! `--force`'s signal is best-effort: this build has no mechanism recording
//! a running `eval`'s process id anywhere `stop` can read it back from (that
//! is a `cmd_eval` change, out of this task's scope), so today `--force`
//! always reports "no eval running" and falls straight through to reporting
//! repository state. The stop request itself is written regardless, so a
//! well-behaved agent still stops at its next verdict either way.

use crate::args::Args;
use autor3search::{gitx, state};
use std::path::Path;

/// The pid file a future `cmd_eval` could write inside the run's state
/// directory while an experiment is in flight, naming the `eval` process to
/// signal. Nothing writes this file today; see the module doc comment.
const EVAL_PID_FILE: &str = "eval.pid";

pub fn run(argv: &[String]) -> i32 {
    let args = match Args::parse(
        argv,
        &[
            ("C", true),
            ("tag", true),
            ("clear", false),
            ("force", false),
        ],
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return autor3search::EXIT_USAGE;
        }
    };
    if args.flag("clear") && args.flag("force") {
        eprintln!("autor3search-rust stop: --clear and --force are opposites; pass one or neither");
        return autor3search::EXIT_USAGE;
    }
    match stop(&args) {
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

/// Resolves which run to signal, matching `status`'s resolution: an explicit
/// `--tag`, or the tag the current branch encodes.
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

fn stop(args: &Args) -> Result<String, String> {
    let root = gitx::root(&args.dir())?;
    let tag = resolve_tag(&root, args.value("tag"))?;
    let dir = state::state_dir(&root, &tag)?;

    if args.flag("clear") {
        state::clear_stop(&dir)?;
        return Ok(format!(
            "stop request cleared for run {tag:?}\nthe agent will keep experimenting; run \
             `autor3search-rust stop` again to stop it\n"
        ));
    }

    state::request_stop(&dir)?;
    let mut out = format!(
        "stop requested for run {tag:?}\nthe current experiment will finish and be scored; the \
         agent will exit the loop after it\n"
    );
    if !args.flag("force") {
        out.push_str(
            "to cancel:      autor3search-rust stop --clear\nto stop sooner: autor3search-rust \
             stop --force\n",
        );
        return Ok(out);
    }

    out.push('\n');
    out.push_str(&force_signal(&dir));
    out.push_str(&repo_state(&root)?);
    Ok(out)
}

/// Best-effort SIGTERM to a recorded `eval` pid — see the module doc comment
/// for why this almost always reports "no eval running" in this build.
fn force_signal(dir: &Path) -> String {
    match read_eval_pid(dir) {
        Some(pid) if process_alive(pid) => {
            let mut out = format!("signalling eval (pid {pid})...\n");
            if terminate(pid) {
                out.push_str("sent SIGTERM\n");
            } else {
                out.push_str("failed to signal the process; it may have already exited\n");
            }
            out
        }
        _ => "no eval running — nothing to signal\n".to_string(),
    }
}

fn read_eval_pid(dir: &Path) -> Option<u32> {
    std::fs::read_to_string(dir.join(EVAL_PID_FILE))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 sends nothing; it only probes whether the pid exists
    // and is signalable by this process.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(unix)]
fn terminate(pid: u32) -> bool {
    // SAFETY: same as `process_alive` — sending a real signal to a pid we
    // just confirmed is alive and ours to signal.
    unsafe { libc::kill(pid as i32, libc::SIGTERM) == 0 }
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    false
}

#[cfg(not(unix))]
fn terminate(_pid: u32) -> bool {
    false
}

/// Tells the human what a forced stop leaves behind. Never changes the
/// repository: dropping a commit is the human's call, and an experiment
/// abandoned mid-flight may still be worth keeping by hand.
fn repo_state(root: &Path) -> Result<String, String> {
    let mut out = String::new();
    out.push_str("repository state:\n");
    let branch = gitx::current_branch(root)?;
    out.push_str(&format!("  {:<10} {branch}\n", "branch"));
    if let Ok(commit) = gitx::head_commit(root) {
        out.push_str(&format!("  {:<10} {commit}\n", "HEAD"));
    }
    out.push_str(
        "\nif the agent had already committed the experiment it was running, that commit \
         carries no verdict. To drop it:  git reset --hard HEAD~1\nto resume this run later: \
         autor3search-rust stop --clear\n",
    );
    Ok(out)
}
