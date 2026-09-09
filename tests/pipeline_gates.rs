//! Every gate in the eval pipeline, exercised through the library so a gate
//! failure is attributable to the gate rather than to a process boundary.

use autor3search::{config, freeze, pipeline, state, verdict};
use std::path::Path;

mod common;
use common::TestRepo;

/// Sets a repo up through baseline and returns everything eval needs.
///
/// The state directory is located by walking `repo`'s own state home
/// (`common::state_dir`) rather than by calling `state::state_dir` directly
/// in-process: that function reads `AUTOR3SEARCH_RUST_STATE_HOME` from the
/// current process's environment, which is only ever set for the *child*
/// process `run_cli` spawns, and is a global these tests must not race on.
fn ready(repo: &TestRepo) -> (config::Config, state::Baseline, std::path::PathBuf) {
    common::run_cli(repo, &["init"]);
    common::git(repo.path(), &["add", "-A"]);
    common::git(repo.path(), &["commit", "-qm", "init"]);
    assert!(
        common::run_cli(repo, &["baseline", "--tag", "t1"])
            .status
            .success()
    );
    let root = repo.path().to_path_buf();
    let dir = common::state_dir(repo, "t1");
    let cfg = config::Config::load(&root.join(config::CONFIG_PATH)).unwrap();
    let base = state::Baseline::load(&dir.join(state::BASELINE_FILE)).unwrap();
    (cfg, base, dir)
}

fn eval_now(
    repo: &TestRepo,
    cfg: config::Config,
    base: state::Baseline,
    dir: &Path,
) -> verdict::VerdictResult {
    let mut o = pipeline::Options {
        root: repo.path().to_path_buf(),
        state_dir: dir.to_path_buf(),
        cfg,
        baseline: base,
    };
    pipeline::eval(&mut o, None)
        .expect("eval must not be a harness error")
        .0
}

#[test]
fn an_out_of_scope_edit_fails_before_anything_is_built() {
    let repo = TestRepo::demo();
    let (mut cfg, base, dir) = ready(&repo);
    cfg.scope = vec!["src/**".to_string()];
    std::fs::write(repo.path().join("outside.rs"), "pub fn x() {}\n").unwrap();
    let r = eval_now(&repo, cfg, base, &dir);
    assert_eq!(r.status, verdict::Status::Fail);
    assert_eq!(r.reason, verdict::Reason::ScopeViolation);
}

#[test]
fn editing_cargo_toml_fails_regardless_of_scope() {
    let repo = TestRepo::demo();
    let (mut cfg, base, dir) = ready(&repo);
    cfg.scope = vec!["**".to_string()]; // even with everything in scope
    let p = repo.path().join("Cargo.toml");
    let mut text = std::fs::read_to_string(&p).unwrap();
    text.push_str("\n[profile.bench]\nopt-level = 3\n");
    std::fs::write(&p, text).unwrap();
    let r = eval_now(&repo, cfg, base, &dir);
    assert_eq!(r.status, verdict::Status::Fail);
    assert_eq!(r.reason, verdict::Reason::ScopeViolation);
    assert!(r.message.contains("Cargo.toml"), "{}", r.message);
}

// The Rust-specific cheat vector: a real speedup with no logic change at all.
#[test]
fn adding_rustflags_via_cargo_config_fails() {
    let repo = TestRepo::demo();
    let (mut cfg, base, dir) = ready(&repo);
    cfg.scope = vec!["**".to_string()];
    std::fs::create_dir_all(repo.path().join(".cargo")).unwrap();
    std::fs::write(
        repo.path().join(".cargo/config.toml"),
        "[build]\nrustflags = [\"-C\", \"target-cpu=native\"]\n",
    )
    .unwrap();
    let r = eval_now(&repo, cfg, base, &dir);
    assert_eq!(r.reason, verdict::Reason::ScopeViolation);
    assert!(r.message.contains("compiler flags"), "{}", r.message);
}

#[test]
fn editing_the_config_fails_with_config_changed_not_scope() {
    let repo = TestRepo::demo();
    let (cfg, base, dir) = ready(&repo);
    let p = repo.path().join(config::CONFIG_PATH);
    let text = std::fs::read_to_string(&p)
        .unwrap()
        .replace("max_regress_pct: 5", "max_regress_pct: 500");
    std::fs::write(&p, text).unwrap();
    let r = eval_now(&repo, cfg, base, &dir);
    assert_eq!(r.reason, verdict::Reason::ConfigChanged);
}

#[test]
fn an_edited_integration_test_is_restored_rather_than_rejected() {
    let repo = TestRepo::demo();
    let (cfg, base, dir) = ready(&repo);
    let p = repo.path().join("tests/wordcount.rs");
    std::fs::write(&p, "#[test] fn weak() { assert!(true); }\n").unwrap();
    let _ = eval_now(&repo, cfg, base, &dir);
    let restored = std::fs::read_to_string(&p).unwrap();
    assert!(
        restored.contains("folds_case") || restored.contains("hello"),
        "{restored}"
    );
}

// C1: the idiomatic large-crate layout — `#[cfg(test)] mod tests;` in
// lib.rs, with the actual assertions living in `src/tests.rs`, which carries
// no `#[cfg(test)]` of its own — must be caught exactly like the inline form
// is. Before the fix, gutting `src/tests.rs` changed nothing `lib.rs`'s own
// digest could see.
#[test]
fn an_out_of_line_cfg_test_module_is_caught_when_gutted() {
    let repo = TestRepo::demo();
    common::run_cli(&repo, &["init"]);

    let lib_path = repo.path().join("src/lib.rs");
    let lib_text = std::fs::read_to_string(&lib_path).unwrap();
    let split = lib_text
        .split("#[cfg(test)]")
        .next()
        .expect("lib.rs must declare a #[cfg(test)] module")
        .to_string()
        + "#[cfg(test)]\nmod tests;\n";
    std::fs::write(&lib_path, split).unwrap();
    let real_test = "use super::*;\n\n#[test]\nfn counts_repeated_words() {\n    let got = \
         count_words(\"the quick brown the\");\n    assert_eq!(got[\"the\"], 2);\n    \
         assert_eq!(got[\"quick\"], 1);\n    assert_eq!(got[\"brown\"], 1);\n    \
         assert_eq!(got.len(), 3);\n}\n";
    std::fs::write(repo.path().join("src/tests.rs"), real_test).unwrap();

    common::git(repo.path(), &["add", "-A"]);
    common::git(
        repo.path(),
        &["commit", "-qm", "split the test module out of line"],
    );
    assert!(
        common::run_cli(&repo, &["baseline", "--tag", "t1"])
            .status
            .success()
    );
    let root = repo.path().to_path_buf();
    let dir = common::state_dir(&repo, "t1");
    let cfg = config::Config::load(&root.join(config::CONFIG_PATH)).unwrap();
    let base = state::Baseline::load(&dir.join(state::BASELINE_FILE)).unwrap();

    // Gut the assertion in the separate file — lib.rs is untouched.
    std::fs::write(
        repo.path().join("src/tests.rs"),
        "use super::*;\n\n#[test]\nfn counts_repeated_words() { assert!(true); }\n",
    )
    .unwrap();

    let r = eval_now(&repo, cfg, base, &dir);
    assert_eq!(r.status, verdict::Status::Fail);
    assert_eq!(r.reason, verdict::Reason::InlineTestModified);
    assert!(r.message.contains("src/lib.rs"), "{}", r.message);
}

// I2: `.gitignore` hiding `.cargo/` from git makes the git-diff-based
// locked-file check blind to a `.cargo/config.toml` created afterward —
// the direct-stat check must still catch it.
#[test]
fn a_gitignored_cargo_config_is_still_caught_by_the_direct_stat_check() {
    let repo = TestRepo::demo();
    let (mut cfg, base, dir) = ready(&repo);
    cfg.scope = vec!["**".to_string()];

    let gitignore = repo.path().join(".gitignore");
    let mut text = std::fs::read_to_string(&gitignore).unwrap();
    text.push_str("\n.cargo/\n");
    std::fs::write(&gitignore, text).unwrap();

    std::fs::create_dir_all(repo.path().join(".cargo")).unwrap();
    std::fs::write(
        repo.path().join(".cargo/config.toml"),
        "[build]\nrustflags = [\"-C\", \"target-cpu=native\"]\n",
    )
    .unwrap();

    let r = eval_now(&repo, cfg, base, &dir);
    assert_eq!(r.status, verdict::Status::Fail);
    assert_eq!(r.reason, verdict::Reason::ScopeViolation);
    assert!(r.message.contains(".cargo/config.toml"), "{}", r.message);
}

// The Rust-specific half of the freeze gate.
#[test]
fn weakening_an_inline_test_fails_with_inline_test_modified() {
    let repo = TestRepo::demo();
    let (cfg, base, dir) = ready(&repo);
    let p = repo.path().join("src/lib.rs");
    let text = std::fs::read_to_string(&p)
        .unwrap()
        .replace("assert_eq!(got[\"the\"], 2);", "assert!(true);");
    std::fs::write(&p, text).unwrap();
    let r = eval_now(&repo, cfg, base, &dir);
    assert_eq!(r.status, verdict::Status::Fail);
    assert_eq!(r.reason, verdict::Reason::InlineTestModified);
    assert!(r.message.contains("src/lib.rs"), "{}", r.message);
}

#[test]
fn adding_a_new_test_file_fails() {
    let repo = TestRepo::demo();
    let (cfg, base, dir) = ready(&repo);
    std::fs::write(repo.path().join("tests/easier.rs"), "#[test] fn t() {}\n").unwrap();
    let r = eval_now(&repo, cfg, base, &dir);
    assert_eq!(r.reason, verdict::Reason::NewTestFile);
}

#[test]
fn a_build_failure_is_a_crash_not_a_fail() {
    let repo = TestRepo::demo();
    let (cfg, base, dir) = ready(&repo);
    let text = std::fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    // A NAME-RESOLUTION error, not a syntax error: `syn` (and so the inline-
    // test gate, which parses this same file to hash the frozen test module
    // and doctest) accepts it just fine — only `cargo build`'s real
    // compilation catches it. Genuinely unparseable source would instead be
    // reported as a changed inline test, since the gate that hashes it runs
    // before the build gate does.
    let broken = text.replace(
        "let mut counts = HashMap::new();",
        "let mut counts = HashMap::new(); this_function_does_not_exist();",
    );
    std::fs::write(repo.path().join("src/lib.rs"), broken).unwrap();
    let r = eval_now(&repo, cfg, base, &dir);
    assert_eq!(r.status, verdict::Status::Crash);
    assert_eq!(r.reason, verdict::Reason::BuildFailed);
}

#[test]
fn deleting_the_work_the_benchmark_measures_fails_the_tests() {
    let repo = TestRepo::demo();
    let (cfg, base, dir) = ready(&repo);
    let text = std::fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    // Keep the signature and the inline test module; gut only the body.
    let gutted = text.replace(
        "let mut counts = HashMap::new();",
        "return HashMap::new(); #[allow(unreachable_code)] let mut counts = HashMap::new();",
    );
    std::fs::write(repo.path().join("src/lib.rs"), gutted).unwrap();
    let r = eval_now(&repo, cfg, base, &dir);
    assert_eq!(r.status, verdict::Status::Fail);
    assert_eq!(r.reason, verdict::Reason::TestsFailed);
}

// Making the BASELINE slow is a cheaper win than making the candidate fast.
#[test]
fn a_moved_baseline_worktree_head_is_detected() {
    let repo = TestRepo::demo();
    let (cfg, mut base, dir) = ready(&repo);
    base.measure_commit = "0000000".to_string();
    let r = eval_now(&repo, cfg, base, &dir);
    assert_eq!(r.reason, verdict::Reason::BaselineTampered);
}

#[test]
fn a_tampered_frozen_store_fails_rather_than_restoring_the_weakened_copy() {
    let repo = TestRepo::demo();
    let (cfg, base, dir) = ready(&repo);
    std::fs::write(
        dir.join(freeze::STORE_DIR).join("tests/wordcount.rs"),
        "#[test] fn weak() { assert!(true); }\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("tests/wordcount.rs"),
        "#[test] fn other() {}\n",
    )
    .unwrap();
    let r = eval_now(&repo, cfg, base, &dir);
    assert_eq!(r.reason, verdict::Reason::FrozenStoreTampered);
}
