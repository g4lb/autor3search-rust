//! Shared fixture for the command-level integration tests.

use std::path::{Path, PathBuf};
use std::process::Command;

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
