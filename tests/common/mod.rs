//! Shared fixture for the command-level integration tests.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

/// Runs the compiled binary against `repo`, pointed at its own out-of-tree
/// state home so tests never touch the developer's real cache.
///
/// Not every test file that pulls in this module uses it — `cmd_init.rs`
/// exercises `init` through its own local helper — so it carries
/// `#[allow(dead_code)]` rather than forcing every importer to silence the
/// warning itself.
#[allow(dead_code)]
pub fn run_cli(repo: &TestRepo, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_autor3search-rust"))
        .args(args)
        .arg("-C")
        .arg(repo.path())
        .env("AUTOR3SEARCH_RUST_STATE_HOME", repo.state_home())
        .output()
        .expect("run command")
}

/// Starts the compiled binary against `repo` WITHOUT waiting for it, for a
/// test that needs to interact with a still-running command — e.g. sending
/// it `stop --force` while it is in the middle of an experiment. Stdout and
/// stderr are piped so the caller can read them once the child exits, the
/// same as `run_cli`'s captured `Output`.
///
/// Only the `stop --force` abort test in `tests/cmd_eval.rs` uses this
/// today, so it carries `#[allow(dead_code)]` like every other helper here.
#[allow(dead_code)]
pub fn spawn_cli(repo: &TestRepo, args: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_autor3search-rust"))
        .args(args)
        .arg("-C")
        .arg(repo.path())
        .env("AUTOR3SEARCH_RUST_STATE_HOME", repo.state_home())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn command")
}

/// Runs a git command in `root`, panicking on failure. For test setup only.
#[allow(dead_code)]
pub fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Locates `repo`'s out-of-tree state directory for `tag` by walking its
/// state home on disk, rather than calling `autor3search::state::state_dir`
/// directly.
///
/// That function derives the directory from `AUTOR3SEARCH_RUST_STATE_HOME`
/// in the CURRENT process's environment, which `run_cli` only ever sets for
/// the child process it spawns. Test threads share one process, so setting
/// that variable for a direct, in-process call would race against every
/// other test doing the same thing concurrently on the same global — and
/// setting it once and leaving it set would make every test depend on
/// whichever repository happened to set it last. Walking the directory tree
/// `baseline` already wrote needs no such shared, mutable global, so it is
/// safe under any test order or thread count.
#[allow(dead_code)]
pub fn state_dir(repo: &TestRepo, tag: &str) -> PathBuf {
    let home = repo.state_home();
    for entry in std::fs::read_dir(home).expect("state home") {
        let p = entry.unwrap().path().join(tag);
        if p.exists() {
            return p;
        }
    }
    panic!("no state dir for tag {tag:?} under {}", home.display());
}

/// Every path under `root`, paired with its mtime, in a stable order.
///
/// Used to prove a command wrote nothing: two snapshots taken either side of
/// a call are `assert_eq!`-compared wholesale, so a changed mtime on an
/// untouched file, a file appearing, or a file disappearing all show up as a
/// diff without either side of the test needing to know in advance which
/// path a bug might have touched.
///
/// Not every test file that pulls in this module uses it, so it carries
/// `#[allow(dead_code)]` like the rest of this file's helpers.
#[allow(dead_code)]
pub fn tree_snapshot(root: &Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        out.push((rel, mtime));
        if meta.is_dir() {
            walk(root, &path, out);
        }
    }
}

pub struct TestRepo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    _state: tempfile::TempDir,
}

impl TestRepo {
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// The state home this repo's runs use, so tests never touch the
    /// developer's real cache. Not read by `cmd_init` tests (`init` writes
    /// only into the repository); kept for the command-level tests that
    /// exercise state under `AUTOR3SEARCH_RUST_STATE_HOME`.
    #[allow(dead_code)]
    pub fn state_home(&self) -> &Path {
        self._state.path()
    }

    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn new_repo() -> (tempfile::TempDir, PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        Self::git(&root, &["init", "-q", "-b", "main"]);
        Self::git(&root, &["config", "user.name", "Test"]);
        Self::git(&root, &["config", "user.email", "test@example.com"]);
        (dir, root, tempfile::tempdir().unwrap())
    }

    /// A copy of `testdata/demo`, committed.
    pub fn demo() -> TestRepo {
        let (dir, root, state) = Self::new_repo();
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/demo");
        copy_dir(&src, &root);
        Self::git(&root, &["add", "-A"]);
        Self::git(&root, &["commit", "-qm", "demo"]);
        TestRepo {
            _dir: dir,
            root,
            _state: state,
        }
    }

    /// A crate with source and tests but no bench target at all.
    ///
    /// Only `cmd_init.rs` uses this today, so every other test crate that
    /// pulls in this module sees it as unused; `#[allow(dead_code)]` for the
    /// same reason as `run_cli` and `git` above.
    #[allow(dead_code)]
    pub fn without_benchmarks() -> TestRepo {
        let (dir, root, state) = Self::new_repo();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"nobench\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn f() -> u8 { 1 }\n").unwrap();
        Self::git(&root, &["add", "-A"]);
        Self::git(&root, &["commit", "-qm", "nobench"]);
        TestRepo {
            _dir: dir,
            root,
            _state: state,
        }
    }
}

fn copy_dir(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        // Never copy build output or a lock file into the fixture.
        if name == "target" || name == "Cargo.lock" {
            continue;
        }
        let dst = to.join(&name);
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&dst).unwrap();
            copy_dir(&entry.path(), &dst);
        } else {
            std::fs::copy(entry.path(), &dst).unwrap();
        }
    }
}
