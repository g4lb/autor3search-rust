use std::process::Command;

#[allow(dead_code)]
mod common;
use common::TestRepo;

fn run(repo: &TestRepo, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_autor3search-rust"))
        .args(args)
        .arg("-C")
        .arg(repo.path())
        .env("AUTOR3SEARCH_RUST_STATE_HOME", repo.state_home())
        .output()
        .expect("run command")
}

fn initialized() -> TestRepo {
    let repo = TestRepo::demo();
    assert!(run(&repo, &["init"]).status.success());
    Command::new("git")
        .args(["add", "-A"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-qm", "init"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    repo
}

#[test]
fn baseline_creates_the_branch_freezes_tests_and_pins_a_worktree() {
    let repo = initialized();
    let out = run(&repo, &["baseline", "--tag", "t1"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&branch.stdout).trim(),
        "autor3search-rust/t1"
    );

    let state = find_state_dir(repo.state_home(), "t1");
    assert!(state.join("baseline.json").exists());
    assert!(state.join("frozen/manifest.json").exists());
    assert!(
        state.join("frozen/tests/wordcount.rs").exists(),
        "tests/ must be frozen"
    );
    assert!(
        state.join("frozen/benches/wordcount.rs").exists(),
        "benches/ must be frozen"
    );
    assert!(state.join("baseline-worktree/Cargo.toml").exists());

    // The inline half of the freeze gate must be recorded too.
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(state.join("frozen/manifest.json")).unwrap())
            .unwrap();
    assert!(
        manifest["inline"]["src/lib.rs"]["cfg_test"].is_string(),
        "inline hashes must cover in-scope sources: {manifest}"
    );
}

#[test]
fn baseline_records_both_commits_equal_at_the_start() {
    let repo = initialized();
    run(&repo, &["baseline", "--tag", "t1"]);
    let state = find_state_dir(repo.state_home(), "t1");
    let b: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(state.join("baseline.json")).unwrap())
            .unwrap();
    assert_eq!(b["commit"], b["measure_commit"]);
    assert!(!b["config_sha256"].as_str().unwrap().is_empty());
}

// A baseline pinned against what is on disk rather than what is in git would
// not be reproducible.
#[test]
fn baseline_refuses_a_dirty_tree() {
    let repo = initialized();
    std::fs::write(repo.path().join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let out = run(&repo, &["baseline", "--tag", "t1"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("uncommitted"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn baseline_refuses_a_reused_tag() {
    let repo = initialized();
    assert!(run(&repo, &["baseline", "--tag", "t1"]).status.success());
    let out = run(&repo, &["baseline", "--tag", "t1"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("t1"));
}

#[test]
fn baseline_refuses_a_traversal_tag() {
    let repo = initialized();
    let out = run(&repo, &["baseline", "--tag", "../escape"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("../escape"), "must name the tag: {stderr}");
    assert!(
        stderr.contains("letters, digits"),
        "must explain the allowed character set: {stderr}"
    );
}

// A leading dash is deliberately NOT rejected: `state::valid_tag` accepts it
// because the raw tag never reaches git (or any other subprocess) as a bare
// argument — it only ever reaches `branch_name` (prefixed) and `state_dir`
// (an absolute path). This documents that intended behaviour rather than
// leaving it implicit.
#[test]
fn baseline_accepts_a_dash_leading_tag() {
    let repo = initialized();
    let out = run(&repo, &["baseline", "--tag", "-rf"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&branch.stdout).trim(),
        "autor3search-rust/-rf"
    );
}

// A failure partway through must not leave wreckage that a retry misreads as
// a naming collision: this pre-occupies the worktree's target path so
// `git worktree add` fails after the branch was already created and the
// tests already frozen, then checks the repository was rolled all the way
// back — original branch restored, run branch gone, state directory gone —
// and that the ORIGINAL failure (not some rollback complaint) is what is
// reported.
#[test]
fn baseline_rolls_back_a_failure_after_the_branch_is_created() {
    let repo = initialized();

    let original_branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    let original_branch = String::from_utf8_lossy(&original_branch.stdout)
        .trim()
        .to_string();

    // Pre-compute where the worktree would land, and occupy it with an
    // ordinary file so `git worktree add` fails there.
    let home = repo.state_home();
    std::fs::create_dir_all(home).unwrap();
    // The state dir is keyed by a hash of the repo path, unknown up front, so
    // instead pre-occupy every possible worktree path is impractical.
    // Instead, run once to discover the state dir, delete the baseline so the
    // tag looks unused again, then occupy the worktree path it just used.
    assert!(run(&repo, &["baseline", "--tag", "t3"]).status.success());
    let state = find_state_dir(home, "t3");
    let worktree_path = state.join("baseline-worktree");

    // Undo that successful run by hand, as if it had never happened, so the
    // real attempt below starts from the same state as a first try: back on
    // the original branch, the run branch and state gone.
    Command::new("git")
        .args(["checkout", "-q", &original_branch])
        .current_dir(repo.path())
        .output()
        .unwrap();
    Command::new("git")
        .args(["branch", "-D", "autor3search-rust/t3"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    std::fs::remove_dir_all(&state).unwrap();

    // Now occupy the exact path `add_worktree` will try to use, with a
    // plain file rather than a worktree, so `git worktree add` fails there.
    std::fs::create_dir_all(worktree_path.parent().unwrap()).unwrap();
    std::fs::write(&worktree_path, b"occupied\n").unwrap();

    let out = run(&repo, &["baseline", "--tag", "t3"]);
    assert!(
        !out.status.success(),
        "baseline must fail when the worktree path is occupied"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("worktree"),
        "the ORIGINAL failure must be reported: {stderr}"
    );
    // Our own branch-collision message is "branch <name> already exists —
    // pick a new tag" (git's own "'<path>' already exists" for the occupied
    // worktree path is fine, and expected, here).
    assert!(
        !stderr.contains("pick a new tag"),
        "must report the ORIGINAL worktree failure, not misdiagnose it as a naming \
         collision: {stderr}"
    );

    // The repository must be back where it started.
    let branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&branch.stdout).trim(),
        original_branch,
        "must be checked back out onto the original branch"
    );

    let branch_exists = Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            "refs/heads/autor3search-rust/t3",
        ])
        .current_dir(repo.path())
        .status()
        .unwrap()
        .success();
    assert!(!branch_exists, "the abandoned run branch must be deleted");

    assert!(
        !state.exists(),
        "the partial state directory must be removed"
    );

    // A retry with the same tag must see a clean slate — the failed attempt
    // above cleared away, not a leftover "branch already exists".
    std::fs::remove_file(&worktree_path).ok();
}

#[test]
fn baseline_refuses_a_repo_with_no_config() {
    let repo = TestRepo::demo();
    let out = run(&repo, &["baseline", "--tag", "t1"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("init"));
}

fn find_state_dir(home: &std::path::Path, tag: &str) -> std::path::PathBuf {
    for entry in std::fs::read_dir(home).expect("state home") {
        let p = entry.unwrap().path().join(tag);
        if p.exists() {
            return p;
        }
    }
    panic!("no state dir for tag {tag} under {}", home.display());
}
