//! Persists a run's state out-of-tree from the repository being optimized.
//!
//! **None of this may live inside the repository.** The agent edits the
//! repository, so in-tree state would be silently writable by the very agent
//! it constrains: it could edit the frozen golden copies, drop a manifest
//! entry, or — worst — edit the pinned baseline WORKTREE to make the BASELINE
//! slow, after which every candidate "improves" and every experiment returns
//! KEEP without optimizing anything.

use crate::config::BenchTarget;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

/// The per-repository state directory name under the user cache.
pub const STATE_DIR_NAME: &str = "autor3search-rust";

/// Relocates every run's out-of-tree state, replacing the user-cache default.
///
/// It exists for the two cases where the user cache is wrong: a container or
/// CI runner with no durable cache, and a test suite, which would otherwise
/// accumulate a directory per temporary repository in the developer's real
/// cache forever.
pub const STATE_HOME_ENV: &str = "AUTOR3SEARCH_RUST_STATE_HOME";

/// Paths within a run's state directory. The frozen store and its manifest
/// also live here, but their constants belong to [`crate::freeze`], which owns
/// that layout.
pub const BASELINE_FILE: &str = "baseline.json";
pub const WORKTREE_NAME: &str = "baseline-worktree";
pub const CRITERION_DIR: &str = "criterion";
pub const PROFILES_DIR: &str = "profiles";
pub const STOP_FILE: &str = "stop";
/// Holds the pid of the currently running `eval`, written on start and
/// removed on exit — see [`claim_eval`]. It answers a different question
/// than [`STOP_FILE`]: that one is a REQUEST the agent reads and acts on at
/// its own pace; this one identifies who to signal when a human cannot wait
/// for that.
pub const EVAL_PID_FILE: &str = "eval.pid";

/// The run branch for a tag.
pub fn branch_name(tag: &str) -> String {
    format!("{STATE_DIR_NAME}/{tag}")
}

/// The tag a run branch encodes, or `None` for any other branch.
pub fn tag_from_branch(branch: &str) -> Option<String> {
    branch
        .strip_prefix(&format!("{STATE_DIR_NAME}/"))
        .filter(|t| !t.is_empty() && !t.contains('/'))
        .map(|t| t.to_string())
}

/// The longest a tag may be. Generous for a run identifier like `sep4` or
/// `2026-09-08`; exists so an over-long tag fails here with a clean message
/// instead of surviving validation and failing later at the first real
/// filesystem write with a raw OS "File name too long" error.
const MAX_TAG_LEN: usize = 64;

/// Validates a run tag against a strict allow-list: letters, digits, `.`, `_`
/// and `-`.
///
/// Notably absent is any path separator, which alone blocks both directory
/// traversal and an absolute path — a tag can never be more than one path
/// segment. `.` and `..` are refused by name so the error can say they are
/// directory references rather than run identifiers.
pub fn valid_tag(tag: &str) -> Result<(), String> {
    if tag.is_empty() {
        return Err("tag must not be empty".to_string());
    }
    if tag == "." || tag == ".." {
        return Err(format!(
            "tag {tag:?} is not allowed: {tag:?} is a directory reference, not a run identifier"
        ));
    }
    if tag.len() > MAX_TAG_LEN {
        return Err(format!(
            "tag {tag:?} is not allowed: tags may be at most {MAX_TAG_LEN} characters long"
        ));
    }
    if !tag
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return Err(format!(
            "tag {tag:?} is not allowed: tags may contain only letters, digits, '.', '_' and '-'"
        ));
    }
    Ok(())
}

/// The out-of-tree directory holding every piece of state the metric depends
/// on, for one repository and run tag.
///
/// Keyed by a hash of the repository's absolute, symlink-resolved path so two
/// checkouts of the same project never share state, and so the same repository
/// reached by two path spellings resolves to one key.
pub fn state_dir(repo_root: &Path, tag: &str) -> Result<PathBuf, String> {
    valid_tag(tag)?;
    let abs = repo_root
        .canonicalize()
        .or_else(|_| std::path::absolute(repo_root))
        .map_err(|e| format!("resolve {}: {e}", repo_root.display()))?;
    let home = state_home()?;
    let mut h = Sha256::new();
    h.update(abs.to_string_lossy().as_bytes());
    let key = format!("{:x}", h.finalize());
    Ok(home.join(&key[..16]).join(tag))
}

/// The directory holding every repository's run state.
///
/// A relative override is refused rather than resolved: the result would
/// depend on the working directory each command was invoked from, so `eval`
/// run from a subdirectory and `stop` run from the repository root would
/// address DIFFERENT state for the same run and the brake would silently miss.
fn state_home() -> Result<PathBuf, String> {
    if let Some(home) = std::env::var_os(STATE_HOME_ENV) {
        let home = PathBuf::from(home);
        if !home.as_os_str().is_empty() {
            if !home.is_absolute() {
                return Err(format!(
                    "{STATE_HOME_ENV} must be an absolute path, got {}: a relative state home \
                     would resolve differently depending on where each command is run from",
                    home.display()
                ));
            }
            return Ok(home);
        }
    }
    let cache = dirs::cache_dir().ok_or("locate user cache dir: none available")?;
    Ok(cache.join(STATE_DIR_NAME))
}

/// The reference point(s) for a run.
///
/// Two distinct reference points, deliberately kept apart:
///
/// - `commit` is the **frozen** anchor: the commit the run started from,
///   recorded once and never changed. The frozen snapshots are relative to it,
///   and the scope gate diffs against it — a fixed anchor there means the gate
///   keeps re-validating the FULL accumulated diff on every eval, rather than
///   trusting that anything already banked as a KEEP must have been in scope.
/// - `measure_commit` is the **advancing** pointer: what the pinned worktree
///   is checked out to and what every eval measures against. It starts equal
///   to `commit` and moves to the candidate's own commit after every KEEP, so
///   each eval answers "did THIS change help", not "is the tree better than
///   when the run started".
///
/// Collapsing them would either freeze the measurement baseline forever —
/// letting a stale early win mask a later no-op as KEEP — or let the scope
/// gate's comparison point drift, letting an agent launder out-of-scope edits
/// into the accepted state. Keep them separate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    pub tag: String,
    pub branch: String,
    /// The FROZEN anchor. Must never change after `baseline` records it.
    pub commit: String,
    /// The ADVANCING measurement anchor. Expected to change over a run.
    #[serde(default)]
    pub measure_commit: String,
    pub created_at: String,
    pub benchmarks: Vec<String>,
    pub bench_targets: Vec<BenchTarget>,
    /// The hash of the in-repo config at baseline time. `config.yaml` stays in
    /// the repository because humans own it, so it is protected by integrity
    /// checking rather than by relocation.
    pub config_sha256: String,
    /// Every locked file (see [`crate::scope::locked_file`]) present at
    /// baseline time, mapped to its hash — populated by statting the
    /// filesystem directly (see [`crate::discover::locked_files`]), not by
    /// asking git what changed, so a gitignored `Cargo.lock` or a `.cargo/`
    /// an agent later adds to `.gitignore` cannot evade the check. `eval`
    /// recomputes this same map and any addition, removal or hash change
    /// is `FAIL(scope_violation)` — independent of, and in addition to, the
    /// git-diff-based locked-file check.
    #[serde(default)]
    pub locked_files: std::collections::BTreeMap<String, String>,
}

impl Baseline {
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let mut json =
            serde_json::to_string_pretty(self).map_err(|e| format!("encode baseline: {e}"))?;
        json.push('\n');
        std::fs::write(path, json).map_err(|e| format!("write {}: {e}", path.display()))
    }

    pub fn load(path: &Path) -> Result<Baseline, String> {
        let raw = match std::fs::read_to_string(path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "no baseline at {}: run 'autor3search-rust baseline --tag <tag>' first",
                    path.display()
                ));
            }
            Err(e) => return Err(format!("read baseline: {e}")),
        };
        let mut b: Baseline = serde_json::from_str(&raw)
            .map_err(|e| format!("parse baseline {}: {e}", path.display()))?;
        if b.measure_commit.is_empty() {
            b.measure_commit = b.commit.clone();
        }
        Ok(b)
    }
}

/// Writes a stop request the agent reads at its next verdict.
pub fn request_stop(state_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(state_dir)
        .map_err(|e| format!("create {}: {e}", state_dir.display()))?;
    std::fs::write(state_dir.join(STOP_FILE), b"requested\n")
        .map_err(|e| format!("write stop request: {e}"))
}

/// Cancels a pending stop. Clearing when nothing is pending is not an error.
pub fn clear_stop(state_dir: &Path) -> Result<(), String> {
    match std::fs::remove_file(state_dir.join(STOP_FILE)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("clear stop request: {e}")),
    }
}

/// Whether a stop has been requested for this run.
pub fn stop_requested(state_dir: &Path) -> bool {
    state_dir.join(STOP_FILE).exists()
}

/// Set when a human's `stop --force` has asked the currently running `eval`
/// to abandon its experiment now, rather than finish it. `eval` installs a
/// `SIGTERM` handler that sets this; the pipeline and [`crate::runner`]
/// check it between gates and between measurement rounds, aborting promptly
/// rather than running to completion first.
///
/// A process-wide static, not something threaded through `pipeline::Options`
/// or `measure::Options`, so that checking it needs no change to either
/// struct's shape — every existing constructor of both keeps compiling
/// unchanged, and nothing outside a real `eval` process (which is the only
/// thing that ever sets it) is affected. It starts `false` in every process
/// and is set at most once, by that process's own signal handler, so it
/// carries no cross-process or cross-test-thread hazard: `cargo test` never
/// sets it, only the compiled binary's own `SIGTERM` handler does, in its
/// own separate process.
pub static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

/// A live claim on a run's `eval.pid` file, held for the life of the current
/// experiment. Dropping releases it.
///
/// On unix the claim is a real, kernel-enforced guarantee: it is an
/// exclusive `flock` held on the open file descriptor, which the kernel
/// releases the moment this process exits **by any means** — a clean
/// return, an early `?`, a panic, or a `SIGKILL` that runs no destructor at
/// all. That is exactly the property [`eval_running`] depends on to tell a
/// live claim from a pid file a crashed `eval` left behind: a bare
/// "does this file exist" check could not offer it, and without it a stale
/// file plus a recycled pid could eventually make `stop --force` signal a
/// process that has nothing to do with this run.
///
/// On non-unix platforms there is no such lock implemented here (see the
/// module's `#[cfg(not(unix))]` fallback): the pid is still written on claim
/// and removed on every exit path this process reaches normally, but a
/// process that dies without unwinding leaves a stale file with no way to
/// tell it apart from a live one by inspecting the file alone. This is a
/// real, disclosed gap on that platform, not a guarantee — see the Task 19
/// report.
#[cfg(unix)]
#[derive(Debug)]
pub struct EvalClaim {
    file: std::fs::File,
    path: PathBuf,
}

#[cfg(unix)]
impl Drop for EvalClaim {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: `self.file` is a valid, open file descriptor for as long
        // as `self` exists.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Claims this run's `eval` slot for the current process. Fails if another
/// live process already holds the claim — two concurrent `eval`s against
/// the same run would fight over the same pinned worktree — naming the
/// incumbent pid rather than silently proceeding.
#[cfg(unix)]
pub fn claim_eval(state_dir: &Path) -> Result<EvalClaim, String> {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::io::AsRawFd;
    std::fs::create_dir_all(state_dir)
        .map_err(|e| format!("create {}: {e}", state_dir.display()))?;
    let path = state_dir.join(EVAL_PID_FILE);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        // Deliberately not truncated on open: another process's content
        // must not be discarded before the lock below confirms nobody else
        // holds the claim. `set_len(0)` truncates explicitly, AFTER that
        // check succeeds.
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    // SAFETY: `file` is a valid, open file descriptor for the duration of
    // this call.
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if !locked {
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        return Err(format!(
            "another autor3search-rust eval (pid {}) is already running for this run — two \
             concurrent evals would fight over the same pinned worktree",
            existing.trim()
        ));
    }
    file.set_len(0)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    writeln!(file, "{}", std::process::id())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    file.flush()
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(EvalClaim { file, path })
}

#[cfg(not(unix))]
#[derive(Debug)]
pub struct EvalClaim {
    path: PathBuf,
}

#[cfg(not(unix))]
impl Drop for EvalClaim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// See [`EvalClaim`]'s doc comment: on this platform the pid is recorded
/// with no lock behind it, so a process that dies without unwinding leaves
/// a file [`eval_running`] cannot tell from a live one.
#[cfg(not(unix))]
pub fn claim_eval(state_dir: &Path) -> Result<EvalClaim, String> {
    std::fs::create_dir_all(state_dir)
        .map_err(|e| format!("create {}: {e}", state_dir.display()))?;
    let path = state_dir.join(EVAL_PID_FILE);
    std::fs::write(&path, format!("{}\n", std::process::id()))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(EvalClaim { path })
}

/// The pid recorded for this run's in-flight `eval`, if any. Does not check
/// liveness — see [`EvalClaim`] and [`eval_running`] for that.
pub fn eval_pid(state_dir: &Path) -> Option<u32> {
    std::fs::read_to_string(state_dir.join(EVAL_PID_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Removes a pid file left behind by an eval that died without releasing
/// its claim, or one killed from outside in a way that skips its own
/// cleanup (Windows `TerminateProcess`; see `cmd_stop`). Removing one that
/// is not there is not an error.
pub fn clear_eval_pid(state_dir: &Path) -> Result<(), String> {
    match std::fs::remove_file(state_dir.join(EVAL_PID_FILE)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("clear eval pid: {e}")),
    }
}

/// The pid of the run's in-flight `eval`, when a LIVE process still holds
/// the claim — distinguishing that from a pid file left behind by one that
/// died without releasing it (a `SIGKILL`, a panic, a crash).
///
/// Answers by attempting a SHARED lock on the same file [`claim_eval`] locks
/// exclusively: taking it succeeds only when nothing holds the exclusive
/// lock, which means the file — if present at all — is a leftover, not a
/// live claim.
#[cfg(unix)]
pub fn eval_running(state_dir: &Path) -> Result<Option<u32>, String> {
    use std::os::unix::io::AsRawFd;
    let path = state_dir.join(EVAL_PID_FILE);
    let Some(pid) = eval_pid(state_dir) else {
        return Ok(None);
    };
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("open {}: {e}", path.display())),
    };
    // SAFETY: `file` is open for the duration of this call.
    let took_shared = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0;
    if took_shared {
        // Nobody held the exclusive lock, so this pid file is a leftover.
        unsafe {
            libc::flock(file.as_raw_fd(), libc::LOCK_UN);
        }
        Ok(None)
    } else {
        Ok(Some(pid))
    }
}

/// No lock-based liveness check is implemented on this platform (see
/// [`EvalClaim`]): this reports the recorded pid as-is, whether or not the
/// process it names is still the one that wrote it.
#[cfg(not(unix))]
pub fn eval_running(state_dir: &Path) -> Result<Option<u32>, String> {
    Ok(eval_pid(state_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn valid_tags_are_accepted() {
        for t in ["sep4", "2026-09-08", "run_1", "a.b"] {
            assert!(valid_tag(t).is_ok(), "{t} should be valid");
        }
    }

    // A tag becomes a directory name, so a path separator alone would let it
    // be more than one segment: traversal and absolute paths both.
    #[test]
    fn a_tag_may_never_contain_a_path_separator_or_be_a_directory_reference() {
        let too_long = "a".repeat(MAX_TAG_LEN + 1);
        for t in [
            "",
            ".",
            "..",
            "../etc",
            "a/b",
            "a\\b",
            "/abs",
            "has space",
            "sep4;rm",
            too_long.as_str(),
        ] {
            assert!(valid_tag(t).is_err(), "{t:?} must be refused");
        }
    }

    #[test]
    fn branch_names_round_trip_with_their_tag() {
        assert_eq!(branch_name("sep4"), "autor3search-rust/sep4");
        assert_eq!(
            tag_from_branch("autor3search-rust/sep4"),
            Some("sep4".to_string())
        );
        assert_eq!(tag_from_branch("main"), None);
        assert_eq!(tag_from_branch("feature/x"), None);
    }

    /// Guards every test that mutates `STATE_HOME_ENV`. Rust runs tests in
    /// threads within one process, so two tests setting a process-wide
    /// environment variable concurrently would interfere with each other.
    /// Rather than relying on `--test-threads=1` (a suite that only passes
    /// serially is not acceptable here), every test that touches the
    /// variable serializes on this lock, held for the full
    /// set/run/unset cycle so no other thread can observe or clobber the
    /// variable in between.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Removes `STATE_HOME_ENV` on drop, including during a panic unwind, so
    /// a test that fails inside `with_home`'s closure never leaks the
    /// variable into whatever test acquires `ENV_LOCK` next.
    struct ClearHomeOnDrop;

    impl Drop for ClearHomeOnDrop {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(STATE_HOME_ENV) };
        }
    }

    fn with_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
        // The lock protects only `()` — access to the environment variable,
        // not any invariant a panicking test could corrupt — so a poisoned
        // lock (left by a *different* test that panicked while holding it)
        // is still safe to use. Without this, one genuine test failure would
        // turn into a PoisonError panic in every other env-var test that
        // runs after it, obscuring which test actually found the bug.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var(STATE_HOME_ENV, home) };
        let _clear = ClearHomeOnDrop;
        f()
    }

    #[test]
    fn state_dir_is_keyed_by_repository_and_tag() {
        let home = tempfile::tempdir().unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        with_home(home.path(), || {
            let da = state_dir(a.path(), "t").unwrap();
            let db = state_dir(b.path(), "t").unwrap();
            let da2 = state_dir(a.path(), "other").unwrap();
            assert_ne!(da, db, "two repositories must not share state");
            assert_ne!(da, da2, "two tags must not share state");
            assert!(da.starts_with(home.path()));
            assert!(da.ends_with("t"));
        });
    }

    #[test]
    fn the_same_repository_by_two_path_spellings_hashes_the_same() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        with_home(home.path(), || {
            let direct = state_dir(repo.path(), "t").unwrap();
            let indirect = state_dir(&repo.path().join("."), "t").unwrap();
            assert_eq!(direct, indirect);
        });
    }

    // A relative override would resolve against whatever directory each
    // command ran from, so eval from a subdirectory and stop from the root
    // would address different state and the brake would silently miss.
    #[test]
    fn a_relative_state_home_is_refused_with_an_explanation() {
        let repo = tempfile::tempdir().unwrap();
        with_home(Path::new("relative/path"), || {
            let err = state_dir(repo.path(), "t").unwrap_err();
            assert!(err.contains("absolute"), "{err}");
        });
    }

    #[test]
    fn a_traversal_tag_never_reaches_a_filesystem_path() {
        let repo = tempfile::tempdir().unwrap();
        assert!(state_dir(repo.path(), "../../etc").is_err());
    }

    fn baseline() -> Baseline {
        Baseline {
            tag: "sep4".into(),
            branch: "autor3search-rust/sep4".into(),
            commit: "a3f1c2d".into(),
            measure_commit: "a3f1c2d".into(),
            created_at: "2026-09-08T12:00:00Z".into(),
            benchmarks: vec!["count_words".into()],
            bench_targets: vec![crate::config::BenchTarget {
                package: "demo".into(),
                target: "wordcount".into(),
            }],
            config_sha256: "0".repeat(64),
            locked_files: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn a_baseline_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(BASELINE_FILE);
        baseline().save(&p).unwrap();
        assert_eq!(Baseline::load(&p).unwrap(), baseline());
    }

    #[test]
    fn a_missing_baseline_says_which_command_to_run() {
        let d = tempfile::tempdir().unwrap();
        let err = Baseline::load(&d.path().join(BASELINE_FILE)).unwrap_err();
        assert!(err.contains("baseline"), "{err}");
    }

    #[test]
    fn stop_requests_are_written_read_and_cleared() {
        let d = tempfile::tempdir().unwrap();
        assert!(!stop_requested(d.path()));
        request_stop(d.path()).unwrap();
        assert!(stop_requested(d.path()));
        clear_stop(d.path()).unwrap();
        assert!(!stop_requested(d.path()));
        // Clearing when nothing is pending is not an error.
        assert!(clear_stop(d.path()).is_ok());
    }

    #[test]
    fn a_claim_records_the_real_pid_and_is_seen_as_running() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(eval_pid(d.path()), None);
        assert_eq!(eval_running(d.path()).unwrap(), None);

        let claim = claim_eval(d.path()).unwrap();
        assert_eq!(eval_pid(d.path()), Some(std::process::id()));
        assert_eq!(eval_running(d.path()).unwrap(), Some(std::process::id()));

        drop(claim);
        assert_eq!(
            eval_pid(d.path()),
            None,
            "dropping the claim removes the pid file"
        );
        assert_eq!(eval_running(d.path()).unwrap(), None);
    }

    // A second claim on the same run must fail, naming the incumbent, not
    // silently overwrite it — two concurrent evals would fight over the
    // same pinned worktree.
    #[test]
    fn a_second_claim_on_the_same_run_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let _first = claim_eval(d.path()).unwrap();
        let err = claim_eval(d.path()).unwrap_err();
        assert!(err.contains(&std::process::id().to_string()), "{err}");
    }

    // A pid file left behind by an eval that died without releasing its
    // claim (no flock held on it) must read as NOT running, not as a live
    // one — this is what lets `stop --force` tell a stale leftover apart
    // from a real process to signal, and it is why the claim is a kernel
    // lock rather than a bare "does this file exist" check.
    #[test]
    fn a_pid_file_with_no_lock_behind_it_is_not_running() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(EVAL_PID_FILE), "999999\n").unwrap();
        assert_eq!(eval_pid(d.path()), Some(999999));
        assert_eq!(
            eval_running(d.path()).unwrap(),
            None,
            "a pid file nobody holds a lock on is a leftover, not a live claim"
        );
    }

    #[test]
    fn clearing_a_pid_file_that_is_not_there_is_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(clear_eval_pid(d.path()).is_ok());
    }

    #[test]
    fn clear_eval_pid_removes_a_leftover_file() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(EVAL_PID_FILE), "123\n").unwrap();
        clear_eval_pid(d.path()).unwrap();
        assert_eq!(eval_pid(d.path()), None);
    }
}
