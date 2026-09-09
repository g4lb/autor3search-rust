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

/// Helper that returns git output without whole-blob trimming.
/// Used for commands with multi-line output that is parsed positionally (e.g., porcelain format),
/// where a leading space is significant and must not be eaten by trim().
fn git_raw(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
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
    Ok(out.stdout)
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
    let mut paths: Vec<String> = Vec::new();

    // Committed changes: use -z to avoid quoting and escaping issues with non-ASCII paths.
    // Split on NUL and drop the final empty entry.
    let diff_output = git_raw(dir, &["diff", "--name-only", "-z", commit])?;
    let diff_str =
        String::from_utf8(diff_output).map_err(|e| format!("git diff output is not UTF-8: {e}"))?;
    for path in diff_str.split('\0') {
        if !path.is_empty() {
            paths.push(path.to_string());
        }
    }

    // Uncommitted and untracked changes: use -z format to avoid quoting and escaping.
    // Format with -z is:
    //   - Most entries: XY path\0 (where XY are two status columns)
    //   - Renames: XY new_path\0 old_path\0
    // We want the NEW path for renames, so we take the first path field after status.
    //
    // --untracked-files=all is load-bearing, not cosmetic: by default `git
    // status` collapses a brand-new, entirely untracked directory into one
    // entry naming just the directory (e.g. `.cargo/`), never the files
    // inside it. A caller matching individual paths against locked-file and
    // scope rules would then see a directory name that matches no rule at
    // all — silently admitting, say, a freshly created `.cargo/config.toml`
    // that was never diffed against anything. Listing every file
    // individually is what makes a new file just as visible as a modified
    // one.
    let status_output = git_raw(
        dir,
        &["status", "--porcelain", "-z", "--untracked-files=all"],
    )?;
    let status_str = String::from_utf8(status_output)
        .map_err(|e| format!("git status output is not UTF-8: {e}"))?;
    let fields: Vec<&str> = status_str.split('\0').collect();
    let mut i = 0;
    while i < fields.len() {
        let field = fields[i];
        if field.len() < 3 {
            i += 1;
            continue;
        }
        // First 3 characters are status codes (2 columns + space).
        let path = &field[3..];
        if !path.is_empty() {
            paths.push(path.to_string());
        }
        // For a rename, the old path is the next field; skip it.
        let status = &field[0..2];
        if status.contains('R') || status.contains('C') {
            i += 2;
        } else {
            i += 1;
        }
    }

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

/// Checks out an existing branch by name.
///
/// Used to put the repository back where it was before
/// [`create_and_checkout_branch`] moved it, when a run fails partway through
/// and has to be rolled back.
pub fn checkout_branch(dir: &Path, name: &str) -> Result<(), String> {
    git(dir, &["checkout", "-q", name])?;
    Ok(())
}

/// Force-deletes a local branch, including one carrying commits not merged
/// anywhere else.
///
/// Used to undo [`create_and_checkout_branch`] on rollback: a failed run may
/// have left the branch with a commit or two of its own (e.g. from the
/// freeze step), and an ordinary `git branch -d` refuses to delete those.
/// Without `-D` here, a rollback could fail to remove the very branch whose
/// continued existence is what makes a retry misreport "branch already
/// exists" instead of surfacing the original failure.
pub fn delete_branch(dir: &Path, name: &str) -> Result<(), String> {
    git(dir, &["branch", "-D", name])?;
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
        // Windows' default `core.autocrlf=true` would let git rewrite these
        // fixtures' checked-out bytes on checkout, so a test comparing exact
        // file content (e.g. after `add_worktree`) would be comparing against
        // git's rewrite rather than what the test itself wrote. A measurement
        // harness wants deterministic bytes from its own fixtures regardless
        // of the host's global git config.
        git(&root, &["config", "core.autocrlf", "false"]);
        git(&root, &["config", "core.eol", "lf"]);
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
        let mut changed = changed_since(&r.root, &base).unwrap();
        changed.sort();
        // Exact set equality: only the two changed files, nothing extra.
        assert_eq!(changed, vec!["a.txt", "untracked.rs"], "{changed:?}");
    }

    // A caller matches every path against locked-file and scope rules
    // individually, so a brand-new, entirely untracked DIRECTORY must report
    // the file inside it, not just the directory's own name — `git status`
    // collapses the latter by default, which would let a new file inside a
    // new directory (a `.cargo/config.toml` an agent just created, say) match
    // no rule at all and pass unnoticed.
    #[test]
    fn changed_since_reports_files_inside_a_new_untracked_directory() {
        let r = repo();
        let base = head_commit(&r.root).unwrap();
        fs::create_dir_all(r.root.join(".cargo")).unwrap();
        fs::write(r.root.join(".cargo/config.toml"), "[build]\n").unwrap();
        let changed = changed_since(&r.root, &base).unwrap();
        assert_eq!(
            changed,
            vec![".cargo/config.toml"],
            "must name the file, not just the new directory: {changed:?}"
        );
    }

    #[test]
    fn branches_are_created_and_detected() {
        let r = repo();
        assert!(!branch_exists(&r.root, "autor3search-rust/t1").unwrap());
        create_and_checkout_branch(&r.root, "autor3search-rust/t1").unwrap();
        assert!(branch_exists(&r.root, "autor3search-rust/t1").unwrap());
        assert_eq!(current_branch(&r.root).unwrap(), "autor3search-rust/t1");
    }

    // Rollback needs to leave a repository exactly as it found it: checked
    // out back on the original branch, and the abandoned run branch gone —
    // even though it carries a commit of its own that an ordinary `git
    // branch -d` would refuse to delete.
    #[test]
    fn a_branch_can_be_checked_out_away_from_and_then_force_deleted() {
        let r = repo();
        create_and_checkout_branch(&r.root, "autor3search-rust/t1").unwrap();
        fs::write(r.root.join("a.txt"), "two\n").unwrap();
        git(
            &r.root,
            &["commit", "-qam", "unmerged work on the run branch"],
        );

        checkout_branch(&r.root, "main").unwrap();
        assert_eq!(current_branch(&r.root).unwrap(), "main");

        delete_branch(&r.root, "autor3search-rust/t1").unwrap();
        assert!(!branch_exists(&r.root, "autor3search-rust/t1").unwrap());
    }

    #[test]
    fn a_worktree_is_pinned_and_can_be_repointed() {
        let r = repo();
        let first = head_commit(&r.root).unwrap();
        fs::write(r.root.join("a.txt"), "two\n").unwrap();
        git(&r.root, &["commit", "-qam", "second"]);
        let second = head_commit(&r.root).unwrap();

        // Give the worktree its own temporary directory, unique to this test,
        // so concurrent tests do not collide on a fixed path in system temp.
        let wt_dir = tempfile::tempdir().unwrap();
        let wt = wt_dir.path().join("wt");
        add_worktree(&r.root, &wt, &first).unwrap();
        assert_eq!(head_commit(&wt).unwrap(), first);
        assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "one\n");

        checkout_detached(&wt, &second).unwrap();
        assert_eq!(head_commit(&wt).unwrap(), second);
        assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "two\n");
    }

    // Regression test for bug 1: leading space eaten by trim() caused spurious paths.
    // A single unstaged edit should report exactly that file, no phantom entry.
    #[test]
    fn changed_since_unstaged_edit_no_phantom_entry() {
        let r = repo();
        let base = head_commit(&r.root).unwrap();
        // Modify the tracked file but do not stage it (unstaged-only = " M a.txt").
        fs::write(r.root.join("a.txt"), "modified\n").unwrap();
        let mut changed = changed_since(&r.root, &base).unwrap();
        changed.sort();
        // Before the fix, the leading space was eaten by trim(), yielding ".txt" as a phantom.
        assert_eq!(changed, vec!["a.txt"], "exact match; no phantom paths");
    }

    // Regression test for bug 2: non-ASCII paths with quoting/escaping.
    // A file with non-ASCII characters should be parsed correctly on both halves.
    #[test]
    fn changed_since_non_ascii_path_matches_exactly() {
        let r = repo();
        let base = head_commit(&r.root).unwrap();

        // Create a file with non-ASCII characters.
        let naive_path = r.root.join("naïve.txt");
        fs::write(&naive_path, "content\n").unwrap();

        // File is both committed-since and currently dirty: modify it.
        git(&r.root, &["add", "-A"]);
        git(&r.root, &["commit", "-qm", "add naïve"]);
        fs::write(&naive_path, "modified\n").unwrap();

        let mut changed = changed_since(&r.root, &base).unwrap();
        changed.sort();

        // Must report the file exactly once, with the correct name (not escaped).
        assert_eq!(changed.len(), 1, "exactly one file changed");
        assert_eq!(changed[0], "naïve.txt", "path matches on-disk name");

        // The returned path should be openable with File::open(repo_root.join(path)).
        let _ = fs::read_to_string(r.root.join(&changed[0]))
            .expect("returned path should open the real file");
    }

    // Regression test: multiple change types with complex scenarios.
    // Renames, deletions, staged, unstaged, untracked all mixed.
    #[test]
    fn changed_since_complex_scenario_exact_set() {
        let r = repo();
        let base = head_commit(&r.root).unwrap();

        // Create initial files for manipulation.
        fs::write(r.root.join("tracked.txt"), "v1\n").unwrap();
        fs::write(r.root.join("to_delete.txt"), "delete me\n").unwrap();
        fs::write(r.root.join("to_rename.txt"), "rename me\n").unwrap();
        git(&r.root, &["add", "-A"]);
        git(&r.root, &["commit", "-qm", "initial"]);

        // Now create a complex state:
        // 1. Modify tracked.txt (unstaged)
        fs::write(r.root.join("tracked.txt"), "v2\n").unwrap();
        // 2. Stage a new file
        fs::write(r.root.join("staged_new.txt"), "new staged\n").unwrap();
        git(&r.root, &["add", "staged_new.txt"]);
        // 3. Create an untracked file
        fs::write(r.root.join("untracked.txt"), "untracked\n").unwrap();
        // 4. Delete to_delete.txt and stage the deletion
        fs::remove_file(r.root.join("to_delete.txt")).unwrap();
        git(&r.root, &["add", "to_delete.txt"]);
        // 5. Rename to_rename.txt and stage the rename
        fs::rename(r.root.join("to_rename.txt"), r.root.join("renamed.txt")).unwrap();
        git(&r.root, &["add", "-A"]);

        let mut changed = changed_since(&r.root, &base).unwrap();
        changed.sort();

        // Expected: tracked.txt (modified, unstaged), staged_new.txt (new, staged),
        // untracked.txt (untracked), to_delete.txt (deleted, staged),
        // renamed.txt (new path from rename), and NO "to_rename.txt" or "->".
        let expected = vec![
            "renamed.txt",
            "staged_new.txt",
            "to_delete.txt",
            "tracked.txt",
            "untracked.txt",
        ];
        assert_eq!(changed, expected, "exact set of changed files");
        assert!(
            !changed.iter().any(|p| p.contains("->")),
            "no '->' in paths"
        );
    }

    // Regression test: filename with spaces should be parsed correctly.
    #[test]
    fn changed_since_filename_with_spaces() {
        let r = repo();
        let base = head_commit(&r.root).unwrap();

        // Create a file with spaces in its name.
        let spaced_path = r.root.join("has space.txt");
        fs::write(&spaced_path, "content\n").unwrap();

        let mut changed = changed_since(&r.root, &base).unwrap();
        changed.sort();

        assert_eq!(changed.len(), 1, "exactly one file");
        assert_eq!(changed[0], "has space.txt", "spaces are preserved");

        // The path should open the real file.
        let _ = fs::read_to_string(r.root.join(&changed[0]))
            .expect("returned path with spaces should open the real file");
    }
}
