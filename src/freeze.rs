//! Snapshots test files at baseline and restores them before every
//! evaluation, so an agent cannot weaken its own success criteria.
//!
//! Paths here are relative to the run's out-of-tree state directory, never to
//! the repository root: the frozen store and its manifest are part of what the
//! metric depends on, so they must live where the agent being measured cannot
//! reach them.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// The frozen store, relative to the state directory.
pub const STORE_DIR: &str = "frozen";
/// The manifest, relative to the state directory.
pub const MANIFEST_PATH: &str = "frozen/manifest.json";

/// Hashes of the test material that cannot be frozen as a file, per in-scope
/// source file. Populated by [`crate::freeze::inline_hashes`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineHashes {
    /// Over the token text of every `#[cfg(test)]`-gated module.
    pub cfg_test: String,
    /// Over every doc-comment line, which is a doctest's whole content.
    pub doc: String,
}

/// Maps repo-relative paths to their hash at baseline time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Frozen whole files: `tests/**` and `benches/**`.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// In-scope source files' inline test hashes.
    #[serde(default)]
    pub inline: BTreeMap<String, InlineHashes>,
}

impl Manifest {
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let mut json =
            serde_json::to_string_pretty(self).map_err(|e| format!("encode manifest: {e}"))?;
        json.push('\n');
        std::fs::write(path, json).map_err(|e| format!("write {}: {e}", path.display()))
    }

    pub fn load(path: &Path) -> Result<Manifest, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("read manifest {}: {e}", path.display()))?;
        serde_json::from_str(&raw).map_err(|e| format!("parse manifest {}: {e}", path.display()))
    }
}

/// What can go wrong while freezing. The three variants are distinguished
/// because [`crate::pipeline`] reports two of them as tampering — a `FAIL`
/// verdict with an actionable message — rather than as a harness malfunction.
#[derive(Debug)]
pub enum FreezeError {
    Io(String),
    /// A symlink somewhere along a frozen file's path. Reads and writes follow
    /// links, so writing to a symlinked destination writes *through* it,
    /// potentially outside the repository entirely.
    Symlink(String),
    /// A golden copy no longer hashes to what the manifest recorded.
    StoreTampered(String),
}

impl std::fmt::Display for FreezeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FreezeError::Io(m) | FreezeError::Symlink(m) | FreezeError::StoreTampered(m) => {
                write!(f, "{m}")
            }
        }
    }
}

impl std::error::Error for FreezeError {}

/// Hex-encoded SHA-256 of some bytes.
pub fn sha256_bytes(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    format!("{:x}", h.finalize())
}

/// Hex-encoded SHA-256 of a file's contents.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(sha256_bytes(&bytes))
}

/// Joins `rel` onto `root`, rejecting anything that would escape it.
///
/// Manifest entries come from a JSON file on disk, so they are untrusted
/// input: `restore` writes through them before every evaluation.
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, FreezeError> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(FreezeError::Io(format!(
            "frozen path {rel:?} must be relative"
        )));
    }
    // Reject a Windows drive prefix or any climb, on every platform, so the
    // check means the same thing everywhere.
    for c in rel_path.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(FreezeError::Io(format!(
                    "frozen path {rel:?} escapes the repository root"
                )));
            }
        }
    }
    Ok(root.join(rel_path))
}

/// The first component of `rel`, beneath `root`, that is a symlink — or
/// `None` when none is.
///
/// `root` itself is deliberately not examined: a repository legitimately
/// reached through a symlinked ancestor (macOS's `/tmp`, a home directory on a
/// linked volume) is not tampering, and refusing to work there would break
/// ordinary setups.
fn symlink_component(root: &Path, rel: &str) -> Result<Option<String>, FreezeError> {
    let mut path = root.to_path_buf();
    let mut seen: Vec<String> = Vec::new();
    for part in Path::new(rel).components() {
        let Component::Normal(part) = part else {
            continue;
        };
        path.push(part);
        seen.push(part.to_string_lossy().into_owned());
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => return Ok(Some(seen.join("/"))),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(FreezeError::Io(format!("stat {}: {e}", path.display()))),
        }
    }
    Ok(None)
}

fn no_symlink(root: &Path, rel: &str, what: &str) -> Result<(), FreezeError> {
    if let Some(link) = symlink_component(root, rel)? {
        return Err(FreezeError::Symlink(format!(
            "{what} {rel}: {link} is a symlink; refusing to read or write through it, which \
             could reach a file outside the repository"
        )));
    }
    Ok(())
}

/// Copies each file into `store_dir` and records its hash. Repo-relative
/// paths are preserved inside the store.
pub fn snapshot(
    repo_root: &Path,
    store_dir: &Path,
    files: &[String],
) -> Result<Manifest, FreezeError> {
    let mut m = Manifest::default();
    for rel in files {
        no_symlink(repo_root, rel, "snapshot")?;
        // A directory left by an earlier attempt under the same tag is not
        // necessarily pristine: refuse to write the golden copy through a link
        // there either.
        no_symlink(store_dir, rel, "snapshot (store)")?;
        let src = safe_join(repo_root, rel)?;
        let dst = safe_join(store_dir, rel)?;
        let bytes = std::fs::read(&src)
            .map_err(|e| FreezeError::Io(format!("snapshot {rel}: read: {e}")))?;
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| FreezeError::Io(format!("snapshot {rel}: mkdir: {e}")))?;
        }
        std::fs::write(&dst, &bytes)
            .map_err(|e| FreezeError::Io(format!("snapshot {rel}: write: {e}")))?;
        m.files.insert(rel.clone(), sha256_bytes(&bytes));
    }
    Ok(m)
}

/// Rewrites every frozen file in the working tree from the store, recreating
/// files the agent deleted. Returns the paths it changed.
///
/// The working tree is examined **before** the store is read, which is what
/// makes the common case — an eval where no test was touched — cost one read
/// per frozen file instead of two. The golden copy is opened, and validated
/// against the manifest hash, only for a file that has to be rewritten.
pub fn restore(
    repo_root: &Path,
    store_dir: &Path,
    m: &Manifest,
) -> Result<Vec<String>, FreezeError> {
    let mut changed = Vec::new();
    for (rel, want_hash) in &m.files {
        // Before any read or write: reads follow links just as writes do, so
        // this has to come first to avoid reading through one and concluding
        // the file is fine.
        no_symlink(repo_root, rel, "restore")?;
        let dst = safe_join(repo_root, rel)?;
        if let Ok(got) = std::fs::read(&dst) {
            if &sha256_bytes(&got) == want_hash {
                continue; // already the frozen content; the store need not be read
            }
        }

        no_symlink(store_dir, rel, "restore (store)")?;
        let src = safe_join(store_dir, rel)?;
        let bytes = std::fs::read(&src)
            .map_err(|e| FreezeError::Io(format!("restore {rel}: read store: {e}")))?;
        let got = sha256_bytes(&bytes);
        if &got != want_hash {
            return Err(FreezeError::StoreTampered(format!(
                "restore {rel}: store copy hashes to {got}, manifest records {want_hash}"
            )));
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| FreezeError::Io(format!("restore {rel}: mkdir: {e}")))?;
        }
        std::fs::write(&dst, &bytes)
            .map_err(|e| FreezeError::Io(format!("restore {rel}: write: {e}")))?;
        changed.push(rel.clone());
    }
    Ok(changed)
}

/// Reports which frozen files currently differ from baseline. A deleted file,
/// and a path with a symlink anywhere along it, both count as changed.
pub fn verify(repo_root: &Path, m: &Manifest) -> Result<Vec<String>, FreezeError> {
    let mut changed = Vec::new();
    for (rel, want_hash) in &m.files {
        if symlink_component(repo_root, rel)?.is_some() {
            // At least as suspicious as a deleted file — report it rather than
            // following the link to read whatever it points at.
            changed.push(rel.clone());
            continue;
        }
        let path = safe_join(repo_root, rel)?;
        match std::fs::read(&path) {
            Ok(bytes) => {
                if &sha256_bytes(&bytes) != want_hash {
                    changed.push(rel.clone());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => changed.push(rel.clone()),
            Err(e) => return Err(FreezeError::Io(format!("verify {rel}: {e}"))),
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: std::path::PathBuf,
        store: std::path::PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let store = dir.path().join("store");
        fs::create_dir_all(repo.join("tests")).unwrap();
        fs::create_dir_all(&store).unwrap();
        fs::write(repo.join("tests/it.rs"), b"assert_eq!(1, 1);\n").unwrap();
        Fixture {
            _dir: dir,
            repo,
            store,
        }
    }

    fn files() -> Vec<String> {
        vec!["tests/it.rs".to_string()]
    }

    #[test]
    fn snapshot_then_restore_round_trips() {
        let f = fixture();
        let m = snapshot(&f.repo, &f.store, &files()).unwrap();
        assert_eq!(m.files.len(), 1);
        fs::write(f.repo.join("tests/it.rs"), b"assert!(true); // weakened\n").unwrap();
        let changed = restore(&f.repo, &f.store, &m).unwrap();
        assert_eq!(changed, vec!["tests/it.rs"]);
        assert_eq!(
            fs::read(f.repo.join("tests/it.rs")).unwrap(),
            b"assert_eq!(1, 1);\n"
        );
    }

    #[test]
    fn restore_recreates_a_deleted_file() {
        let f = fixture();
        let m = snapshot(&f.repo, &f.store, &files()).unwrap();
        fs::remove_file(f.repo.join("tests/it.rs")).unwrap();
        let changed = restore(&f.repo, &f.store, &m).unwrap();
        assert_eq!(changed, vec!["tests/it.rs"]);
        assert!(f.repo.join("tests/it.rs").exists());
    }

    // The common case — no test was touched — must not read the golden copy
    // at all, so restore reports nothing changed.
    #[test]
    fn restore_is_a_no_op_when_nothing_was_touched() {
        let f = fixture();
        let m = snapshot(&f.repo, &f.store, &files()).unwrap();
        assert!(restore(&f.repo, &f.store, &m).unwrap().is_empty());
    }

    #[test]
    fn verify_reports_edited_and_deleted_files() {
        let f = fixture();
        let m = snapshot(&f.repo, &f.store, &files()).unwrap();
        assert!(verify(&f.repo, &m).unwrap().is_empty());
        fs::write(f.repo.join("tests/it.rs"), b"different\n").unwrap();
        assert_eq!(verify(&f.repo, &m).unwrap(), vec!["tests/it.rs"]);
        fs::remove_file(f.repo.join("tests/it.rs")).unwrap();
        assert_eq!(verify(&f.repo, &m).unwrap(), vec!["tests/it.rs"]);
    }

    // A rewritten golden copy would be restored into the working tree by every
    // later eval — the exact outcome freezing exists to prevent — so the store
    // is checked against the manifest hash, not trusted.
    #[test]
    fn a_tampered_store_copy_is_detected() {
        let f = fixture();
        let m = snapshot(&f.repo, &f.store, &files()).unwrap();
        fs::write(f.store.join("tests/it.rs"), b"assert!(true); // weakened\n").unwrap();
        fs::write(f.repo.join("tests/it.rs"), b"anything else\n").unwrap();
        match restore(&f.repo, &f.store, &m) {
            Err(FreezeError::StoreTampered(msg)) => assert!(msg.contains("tests/it.rs"), "{msg}"),
            other => panic!("expected StoreTampered, got {other:?}"),
        }
    }

    #[test]
    fn a_manifest_path_escaping_the_root_is_refused() {
        let f = fixture();
        let mut m = Manifest::default();
        m.files.insert("../escape.rs".into(), "0".repeat(64));
        assert!(restore(&f.repo, &f.store, &m).is_err());
    }

    #[test]
    fn an_absolute_manifest_path_is_refused() {
        let f = fixture();
        let mut m = Manifest::default();
        let abs = if cfg!(windows) {
            "C:\\evil.rs"
        } else {
            "/etc/evil.rs"
        };
        m.files.insert(abs.into(), "0".repeat(64));
        assert!(restore(&f.repo, &f.store, &m).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_test_file_is_refused_by_both_snapshot_and_restore() {
        let f = fixture();
        let outside = f.repo.parent().unwrap().join("outside.rs");
        fs::write(&outside, b"outside\n").unwrap();
        fs::remove_file(f.repo.join("tests/it.rs")).unwrap();
        std::os::unix::fs::symlink(&outside, f.repo.join("tests/it.rs")).unwrap();
        assert!(matches!(
            snapshot(&f.repo, &f.store, &files()),
            Err(FreezeError::Symlink(_))
        ));

        // And on the restore side, where writing through the link would put
        // frozen content outside the repository entirely.
        let f2 = fixture();
        let m = snapshot(&f2.repo, &f2.store, &files()).unwrap();
        fs::remove_file(f2.repo.join("tests/it.rs")).unwrap();
        std::os::unix::fs::symlink(&outside, f2.repo.join("tests/it.rs")).unwrap();
        assert!(matches!(
            restore(&f2.repo, &f2.store, &m),
            Err(FreezeError::Symlink(_))
        ));
    }

    // Checking only the final component is not enough: read and write resolve
    // the whole path, so swapping a PARENT DIRECTORY for a link redirects the
    // write just as effectively, and an lstat on the file then reports a
    // perfectly ordinary regular file.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_parent_directory_is_refused() {
        let f = fixture();
        let m = snapshot(&f.repo, &f.store, &files()).unwrap();
        let elsewhere = f.repo.parent().unwrap().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("it.rs"), b"decoy\n").unwrap();
        fs::remove_dir_all(f.repo.join("tests")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, f.repo.join("tests")).unwrap();
        assert!(matches!(
            restore(&f.repo, &f.store, &m),
            Err(FreezeError::Symlink(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn verify_reports_a_symlinked_path_as_changed_rather_than_following_it() {
        let f = fixture();
        let m = snapshot(&f.repo, &f.store, &files()).unwrap();
        let outside = f.repo.parent().unwrap().join("outside2.rs");
        fs::write(&outside, b"assert_eq!(1, 1);\n").unwrap(); // identical bytes!
        fs::remove_file(f.repo.join("tests/it.rs")).unwrap();
        std::os::unix::fs::symlink(&outside, f.repo.join("tests/it.rs")).unwrap();
        assert_eq!(verify(&f.repo, &m).unwrap(), vec!["tests/it.rs"]);
    }

    #[test]
    fn manifest_saves_and_loads() {
        let f = fixture();
        let m = snapshot(&f.repo, &f.store, &files()).unwrap();
        let path = f.store.join("manifest.json");
        m.save(&path).unwrap();
        assert_eq!(Manifest::load(&path).unwrap(), m);
    }
}
