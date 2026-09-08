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
