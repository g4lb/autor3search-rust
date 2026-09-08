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

    fn with_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var(STATE_HOME_ENV, home) };
        let out = f();
        unsafe { std::env::remove_var(STATE_HOME_ENV) };
        out
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
}
