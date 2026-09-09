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

// I5: `status` used to tell a human to run `baseline --tag <tag> --force`
// to recreate a missing worktree — a command `baseline` always refuses,
// --force or not, because its tag-collision check is unconditional. The
// advice must both (a) not recommend that dead end and (b) name a command
// that actually works.
#[test]
fn status_advises_a_real_fix_for_a_missing_worktree_not_a_dead_end() {
    let repo = ready("t1");
    let dir = common::state_dir(&repo, "t1");
    std::fs::remove_dir_all(dir.join("baseline-worktree")).unwrap();

    let text = String::from_utf8_lossy(&common::run_cli(&repo, &["status"]).stdout).into_owned();
    assert!(text.contains("MISSING"), "{text}");
    assert!(
        !text.contains("re-run `autor3search-rust baseline --tag t1 --force`"),
        "must not recommend a command baseline always refuses:\n{text}"
    );
    assert!(
        text.contains("git") && text.contains("worktree add"),
        "must name the real fix:\n{text}"
    );

    // Prove the old advice really was a dead end: baseline --force still
    // refuses outright for a tag that already has one.
    assert!(
        !common::run_cli(&repo, &["baseline", "--tag", "t1", "--force"])
            .status
            .success(),
        "a tag collision must be refused even with --force"
    );
}

// M7: a stale `eval.pid` — left by an `eval` that died without releasing
// its claim — must be cleared by `stop --force` even when there is nothing
// live to signal, matching the Go original's "keep status honest" behavior.
#[test]
fn stop_force_clears_a_stale_eval_pid_with_nothing_to_signal() {
    let repo = ready("t1");
    let dir = common::state_dir(&repo, "t1");
    std::fs::write(dir.join("eval.pid"), "999999999\n").unwrap();
    assert!(dir.join("eval.pid").exists());

    let out = common::run_cli(&repo, &["stop", "--force"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("nothing to signal"), "{text}");
    assert!(
        !dir.join("eval.pid").exists(),
        "a stale pid file must be cleared, not left behind"
    );
}
