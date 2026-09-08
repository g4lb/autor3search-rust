//! Git plumbing, by shelling out to `git`.
//!
//! No library binding: the Go original shells out too, the surface used here
//! is small and stable, and a linked C library would make cross-compiling the
//! harness harder than the convenience is worth.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("run git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The repository root containing `dir`.
pub fn root(dir: &Path) -> Result<PathBuf, String> {
    Ok(PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"])?))
}

/// The short hash at `HEAD`.
pub fn head_commit(dir: &Path) -> Result<String, String> {
    git(dir, &["rev-parse", "--short", "HEAD"])
}

/// The current branch name, or an error on a detached HEAD.
pub fn current_branch(dir: &Path) -> Result<String, String> {
    let name = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if name == "HEAD" {
        return Err("HEAD is detached; check out the run branch first".to_string());
    }
    Ok(name)
}

/// Whether the working tree has no changes at all.
///
/// Untracked files count as dirty. A baseline pinned against what is on disk
/// rather than what is in git would not be reproducible.
pub fn is_clean(dir: &Path) -> Result<bool, String> {
    Ok(git(dir, &["status", "--porcelain"])?.is_empty())
}

/// Every repo-relative path that differs from `commit`, including
/// uncommitted and untracked work.
///
/// The uncommitted half matters: without it an agent could edit out of scope
/// and simply not commit before calling eval.
pub fn changed_since(dir: &Path, commit: &str) -> Result<Vec<String>, String> {
    let mut paths: Vec<String> = git(dir, &["diff", "--name-only", commit])?
        .lines()
        .map(|s| s.to_string())
        .collect();
    for line in git(dir, &["status", "--porcelain"])?.lines() {
        // Porcelain v1: two status characters, a space, then the path. A
        // rename reads "old -> new"; the new path is what matters here.
        let path = line.get(3..).unwrap_or("").trim();
        let path = path.rsplit(" -> ").next().unwrap_or(path);
        let path = path.trim_matches('"');
        if !path.is_empty() {
            paths.push(path.to_string());
        }
    }
    paths.retain(|p| !p.is_empty());
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Whether a local branch exists.
pub fn branch_exists(dir: &Path, name: &str) -> Result<bool, String> {
    let refname = format!("refs/heads/{name}");
    Ok(Command::new("git")
        .args(["show-ref", "--verify", "--quiet", &refname])
        .current_dir(dir)
        .status()
        .map_err(|e| format!("run git show-ref: {e}"))?
        .success())
}

/// Creates a branch at `HEAD` and checks it out.
pub fn create_and_checkout_branch(dir: &Path, name: &str) -> Result<(), String> {
    git(dir, &["checkout", "-q", "-b", name])?;
    Ok(())
}

/// Adds a detached worktree pinned at `commit`.
pub fn add_worktree(repo: &Path, path: &Path, commit: &str) -> Result<(), String> {
    let path_str = path.to_string_lossy().into_owned();
    git(
        repo,
        &["worktree", "add", "--detach", "-f", &path_str, commit],
    )?;
    Ok(())
}

/// Re-points an existing worktree at `commit`, discarding local changes.
pub fn checkout_detached(worktree: &Path, commit: &str) -> Result<(), String> {
    git(worktree, &["checkout", "-q", "-f", "--detach", commit])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    struct Repo {
        _dir: tempfile::TempDir,
        root: PathBuf,
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

    fn repo() -> Repo {
        let dir = tempfile::tempdir().unwrap();
        // Resolve symlinked ancestors up front (macOS /tmp), so comparisons
        // against gitx::root do not fail on the spelling of the path.
        let root = dir.path().canonicalize().unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "first"]);
        Repo { _dir: dir, root }
    }

    #[test]
    fn root_is_found_from_a_subdirectory() {
        let r = repo();
        let sub = r.root.join("deep/nested");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(root(&sub).unwrap().canonicalize().unwrap(), r.root);
    }

    #[test]
    fn root_of_a_non_repository_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(root(dir.path()).is_err());
    }

    #[test]
    fn head_commit_and_branch_are_reported() {
        let r = repo();
        assert!(!head_commit(&r.root).unwrap().is_empty());
        assert_eq!(current_branch(&r.root).unwrap(), "main");
    }

    #[test]
    fn is_clean_tracks_the_working_tree() {
        let r = repo();
        assert!(is_clean(&r.root).unwrap());
        fs::write(r.root.join("a.txt"), "two\n").unwrap();
        assert!(!is_clean(&r.root).unwrap());
    }

    // Untracked files count as dirty: a baseline pinned against what is on
    // disk rather than what is in git would not be reproducible.
    #[test]
    fn an_untracked_file_makes_the_tree_dirty() {
        let r = repo();
        fs::write(r.root.join("new.txt"), "x\n").unwrap();
        assert!(!is_clean(&r.root).unwrap());
    }

    #[test]
    fn changed_since_lists_paths_added_and_modified_after_a_commit() {
        let r = repo();
        let base = head_commit(&r.root).unwrap();
        fs::write(r.root.join("a.txt"), "two\n").unwrap();
        fs::create_dir_all(r.root.join("src")).unwrap();
        fs::write(r.root.join("src/b.rs"), "fn f() {}\n").unwrap();
        git(&r.root, &["add", "-A"]);
        git(&r.root, &["commit", "-qm", "second"]);
        let mut changed = changed_since(&r.root, &base).unwrap();
        changed.sort();
        assert_eq!(changed, vec!["a.txt", "src/b.rs"]);
    }

    // The scope gate must see uncommitted work too, or an agent could edit
    // out of scope and simply not commit it before eval.
    #[test]
    fn changed_since_includes_uncommitted_and_untracked_work() {
        let r = repo();
        let base = head_commit(&r.root).unwrap();
        fs::write(r.root.join("a.txt"), "dirty\n").unwrap();
        fs::write(r.root.join("untracked.rs"), "fn f() {}\n").unwrap();
        let changed = changed_since(&r.root, &base).unwrap();
        assert!(changed.contains(&"a.txt".to_string()), "{changed:?}");
        assert!(changed.contains(&"untracked.rs".to_string()), "{changed:?}");
    }

    #[test]
    fn branches_are_created_and_detected() {
        let r = repo();
        assert!(!branch_exists(&r.root, "autor3search-rust/t1").unwrap());
        create_and_checkout_branch(&r.root, "autor3search-rust/t1").unwrap();
        assert!(branch_exists(&r.root, "autor3search-rust/t1").unwrap());
        assert_eq!(current_branch(&r.root).unwrap(), "autor3search-rust/t1");
    }

    #[test]
    fn a_worktree_is_pinned_and_can_be_repointed() {
        let r = repo();
        let first = head_commit(&r.root).unwrap();
        fs::write(r.root.join("a.txt"), "two\n").unwrap();
        git(&r.root, &["commit", "-qam", "second"]);
        let second = head_commit(&r.root).unwrap();

        let wt = r.root.parent().unwrap().join("wt");
        add_worktree(&r.root, &wt, &first).unwrap();
        assert_eq!(head_commit(&wt).unwrap(), first);
        assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "one\n");

        checkout_detached(&wt, &second).unwrap();
        assert_eq!(head_commit(&wt).unwrap(), second);
        assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "two\n");
    }
}
