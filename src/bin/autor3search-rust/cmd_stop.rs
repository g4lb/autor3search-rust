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
//! On unix, `--force` sends a real `SIGTERM` to a verified-LIVE `eval`
//! process (see `autor3search::state::eval_running`, which tells a live
//! claim from a pid file a crashed eval left behind) and waits briefly for
//! it to exit on its own — `eval`'s own signal handler sets
//! `state::CANCEL_REQUESTED`, which the pipeline and `Runner::cargo` check
//! between gates and between measurement rounds, so the process gets a
//! chance to record `ABORTED`/`stop_forced` and tear down its own benchmark
//! subprocess cleanly. Windows has no such polite half — see [`terminate`]
//! — so there `--force` ends the process outright, and the killed `eval`
//! never gets to record what it abandoned.

use crate::args::Args;
use autor3search::{gitx, state};
use std::path::Path;
use std::time::Duration;

/// How long `--force` waits for a signalled `eval` to release its claim
/// before giving up and reporting the repository state regardless. Not an
/// escalation to a harder kill signal — see the module doc comment on why a
/// stuck `eval` is a courtesy wait, not something this command tries to
/// clean up further by force.
const FORCE_GRACE: Duration = Duration::from_secs(10);
const FORCE_POLL_INTERVAL: Duration = Duration::from_millis(50);

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

/// Signals a live `eval`, if one holds the run's claim, and reports what
/// happened. See [`terminate`] for the unix/Windows split.
fn force_signal(dir: &Path) -> String {
    let live = match state::eval_running(dir) {
        Ok(p) => p,
        Err(e) => return format!("could not tell whether an eval is running: {e}\n"),
    };
    let Some(pid) = live else {
        // `eval_running` returning `None` can mean either that no pid file
        // exists at all, or that one does but nobody holds its lock — a
        // leftover from an `eval` that died without releasing its claim (a
        // `SIGKILL`, a panic, a crash). Clear it here, as the Go original
        // does, to keep the recorded state honest rather than leaving a
        // known-stale pid file sitting on disk indefinitely. Clearing a
        // file that is not there is not an error.
        let _ = state::clear_eval_pid(dir);
        return "no eval running — nothing to signal\n".to_string();
    };

    let mut out = format!("signalling eval (pid {pid})...\n");
    if !terminate(pid) {
        out.push_str("failed to signal the process; it may have already exited\n");
        return out;
    }

    if cfg!(unix) {
        out.push_str("sent SIGTERM; waiting for it to finish aborting...\n");
        if wait_for_release(dir, FORCE_GRACE) {
            out.push_str("eval exited\n");
        } else {
            out.push_str(&format!(
                "eval did not exit within {:?}; it may still be tearing down its benchmark \
                 subprocess — check again with `autor3search-rust status`\n",
                FORCE_GRACE
            ));
        }
    } else {
        // TerminateProcess is immediate and gives the target no chance to
        // run its own cleanup, so the claim it held has to be cleared here
        // instead — `eval` never gets back to `EvalClaim::drop` to do it
        // itself.
        out.push_str("eval terminated (no graceful shutdown on this platform)\n");
        let _ = state::clear_eval_pid(dir);
    }
    out
}

/// Polls until no live process holds the run's claim, or `grace` runs out.
///
/// Polls [`state::eval_running`] rather than the pid directly: a signalled
/// process that has died but not yet been reaped by its parent is a
/// ZOMBIE, and a liveness probe against the bare pid can still say yes for
/// one. `eval_running`'s claim (a kernel `flock` on unix, an exclusive
/// file-sharing handle on Windows) is released the moment the process
/// exits by any means, zombie or not, so it is the honest signal.
fn wait_for_release(dir: &Path, grace: Duration) -> bool {
    let deadline = std::time::Instant::now() + grace;
    loop {
        match state::eval_running(dir) {
            Ok(None) | Err(_) => return true,
            Ok(Some(_)) => {}
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(FORCE_POLL_INTERVAL);
    }
}

#[cfg(unix)]
fn terminate(pid: u32) -> bool {
    // SAFETY: sends a real signal to a pid `state::eval_running` just
    // confirmed holds this run's live claim.
    unsafe { libc::kill(pid as i32, libc::SIGTERM) == 0 }
}

/// Windows has no `SIGTERM` a process can catch and act on mid-benchmark, so
/// there is no polite half here: this ends the process outright, and it
/// never gets to record what it abandoned or clean up its own claim (see
/// [`force_signal`], which clears it afterward instead).
///
/// UNVERIFIED on this platform: developed without access to a Windows
/// machine. Uses only `Win32_System_Threading` and `Win32_Foundation`,
/// already enabled in `Cargo.toml` for `procgroup_windows.rs`'s job-object
/// machinery — no new dependency or feature. CI covers Windows.
#[cfg(windows)]
fn terminate(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return false;
        }
        let ok = TerminateProcess(handle, 1) != 0;
        CloseHandle(handle);
        ok
    }
}

#[cfg(not(any(unix, windows)))]
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
