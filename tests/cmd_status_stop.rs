mod common;
use common::TestRepo;

fn ready(tag: &str) -> TestRepo {
    let repo = TestRepo::demo();
    common::run_cli(&repo, &["init"]);
    common::git(repo.path(), &["add", "-A"]);
    common::git(repo.path(), &["commit", "-qm", "init"]);
    common::run_cli(&repo, &["baseline", "--tag", tag]);
    repo
}

#[test]
fn status_reports_the_run_and_writes_nothing() {
    let repo = ready("t1");
    let before = common::tree_snapshot(repo.path());
    let out = common::run_cli(&repo, &["status"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    for expected in [
        "run tag",
        "t1",
        "autor3search-rust/t1",
        "baseline",
        "worktree",
        "stop",
    ] {
        assert!(
            text.contains(expected),
            "status must mention {expected:?}:\n{text}"
        );
    }
    // Checking on a run must not be able to change it.
    assert_eq!(common::tree_snapshot(repo.path()), before);
}

#[test]
fn status_works_from_another_branch_with_an_explicit_tag() {
    let repo = ready("t1");
    common::git(repo.path(), &["checkout", "-q", "main"]);
    assert!(
        common::run_cli(&repo, &["status", "--tag", "t1"])
            .status
            .success()
    );
    // Without the tag there is nothing to infer from a non-run branch.
    assert!(!common::run_cli(&repo, &["status"]).status.success());
}

#[test]
fn stop_requests_are_visible_to_status_and_can_be_cleared() {
    let repo = ready("t1");
    assert!(common::run_cli(&repo, &["stop"]).status.success());
    let text = String::from_utf8_lossy(&common::run_cli(&repo, &["status"]).stdout).into_owned();
    assert!(text.contains("requested"), "{text}");

    assert!(
        common::run_cli(&repo, &["stop", "--clear"])
            .status
            .success()
    );
    let text = String::from_utf8_lossy(&common::run_cli(&repo, &["status"]).stdout).into_owned();
    assert!(text.contains("not requested"), "{text}");
}

#[test]
fn stop_tells_the_user_what_it_did_and_what_happens_next() {
    let repo = ready("t1");
    let text = String::from_utf8_lossy(&common::run_cli(&repo, &["stop"]).stdout).into_owned();
    assert!(text.contains("current experiment"), "{text}");
}
