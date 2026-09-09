mod common;
use autor3search::results::{self, Row};
use common::TestRepo;

fn row(status: &str, score: f64) -> Row {
    Row {
        commit: "abc1234".into(),
        score,
        best_bench_delta: -10.0,
        status: status.into(),
        description: "x".into(),
    }
}

#[test]
fn report_counts_by_status_and_multiplies_kept_scores() {
    let repo = TestRepo::demo();
    let p = repo.path().join(results::RESULTS_PATH);
    results::append(&p, &row("KEEP", 0.8)).unwrap();
    results::append(&p, &row("KEEP", 0.5)).unwrap();
    results::append(&p, &row("DISCARD", 1.0)).unwrap();
    results::append(&p, &row("FAIL", 0.0)).unwrap();

    let text = String::from_utf8_lossy(&common::run_cli(&repo, &["report"]).stdout).into_owned();
    assert!(
        text.contains("2 keep") || text.contains("keep    2"),
        "{text}"
    );
    // 0.8 * 0.5 = 0.40 cumulative, i.e. a 2.5x speedup — the PRODUCT, not the
    // latest kept score, because measure_commit advances after every KEEP.
    assert!(
        text.contains("0.40") || text.contains("2.5"),
        "cumulative must compound:\n{text}"
    );
}

#[test]
fn report_with_no_kept_experiments_says_so_rather_than_printing_zero() {
    let repo = TestRepo::demo();
    let p = repo.path().join(results::RESULTS_PATH);
    results::append(&p, &row("DISCARD", 1.0)).unwrap();
    let text = String::from_utf8_lossy(&common::run_cli(&repo, &["report"]).stdout).into_owned();
    assert!(!text.contains("0.0%"), "{text}");
    assert!(
        text.to_lowercase().contains("no experiments were kept"),
        "{text}"
    );
}

#[test]
fn report_on_an_empty_log_is_not_an_error() {
    let repo = TestRepo::demo();
    assert!(common::run_cli(&repo, &["report"]).status.success());
}

#[test]
fn doctor_always_exits_zero_and_prints_findings() {
    let repo = TestRepo::demo();
    let out = common::run_cli(&repo, &["doctor"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "doctor is informational and always exits 0"
    );
    assert!(!String::from_utf8_lossy(&out.stdout).is_empty());
}

#[test]
fn version_reports_a_build() {
    let repo = TestRepo::demo();
    let out = common::run_cli(&repo, &["version"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(env!("CARGO_PKG_VERSION")), "{text}");
}

// samply is not installed on CI, and profile must degrade rather than fail.
#[test]
fn profile_without_samply_explains_how_to_install_it_and_exits_zero() {
    let repo = TestRepo::demo();
    common::run_cli(&repo, &["init"]);
    let out = common::run_cli(&repo, &["profile"]);
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    if !text.contains("samply") {
        return; // samply IS installed here; the happy path is covered elsewhere
    }
    assert_eq!(
        out.status.code(),
        Some(0),
        "profile is advisory and must not fail a run"
    );
    assert!(text.contains("cargo install samply"), "{text}");
}
