use std::path::Path;
use std::process::Command;

mod common;
use common::TestRepo;

// `init` runs a real `cargo bench -- --list` to discover benchmarks, which
// is the single most expensive thing any of these commands do (it compiles
// criterion and its transitive dependencies from scratch the first time).
// Pointing it at the shared target dir every other fixture uses (see
// `common::shared_target_dir`) lets that compile happen once for the whole
// `cargo test` run rather than once per test in this file. `init` never
// builds a second, baseline worktree, so — unlike `eval` — sharing it here
// is safe; see `common::cargo_target_dir_for`'s doc comment for why that
// distinction matters.
fn init(repo: &Path, extra: &[&str]) -> std::process::Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_autor3search-rust"));
    c.arg("init")
        .arg("-C")
        .arg(repo)
        .args(extra)
        .env("CARGO_TARGET_DIR", common::shared_target_dir());
    c.output().expect("run init")
}

#[test]
fn init_writes_config_program_and_gitignore() {
    let repo = TestRepo::demo();
    let out = init(repo.path(), &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let cfg = repo.path().join(".autor3search/config.yaml");
    assert!(cfg.exists(), "config must be written");
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(
        text.contains("count_words"),
        "discovered benchmark must be listed:\n{text}"
    );
    assert!(
        text.contains("package: demo"),
        "bench target must be package-qualified:\n{text}"
    );
    assert!(text.contains("target: wordcount"), "{text}");

    assert!(
        repo.path().join("program.md").exists(),
        "program.md must be written"
    );

    // Ignoring .autor3search/ wholesale would leave the config untracked,
    // which contradicts the whole reason it lives in the repo.
    let ignore = std::fs::read_to_string(repo.path().join(".gitignore")).unwrap();
    assert!(ignore.contains(".autor3search/*"), "{ignore}");
    assert!(ignore.contains("!.autor3search/config.yaml"), "{ignore}");
    assert!(ignore.contains("results.tsv"), "{ignore}");
    assert!(ignore.contains("run.log"), "{ignore}");
}

#[test]
fn init_reports_what_it_discovered() {
    let repo = TestRepo::demo();
    let out = init(repo.path(), &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("count_words"), "{stdout}");
}

#[test]
fn init_refuses_to_overwrite_an_existing_config_without_force() {
    let repo = TestRepo::demo();
    assert!(init(repo.path(), &[]).status.success());
    let out = init(repo.path(), &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--force"));
    assert!(init(repo.path(), &["--force"]).status.success());
}

// The tool optimizes what it can measure and refuses to guess. A config with
// an empty benchmark list would silently optimize nothing.
#[test]
fn init_refuses_a_repository_with_no_benchmarks() {
    let repo = TestRepo::without_benchmarks();
    let out = init(repo.path(), &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no benchmarks"), "{stderr}");
    assert!(!repo.path().join(".autor3search/config.yaml").exists());
}

#[test]
fn init_does_not_commit_anything() {
    let repo = TestRepo::demo();
    init(repo.path(), &[]);
    let out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&out.stdout).is_empty(),
        "init writes files but must leave committing to the human"
    );
}

// Single-dash long flags are a Go-tool spelling. Accepting them silently
// would make a copy-pasted Go instruction appear to work.
#[test]
fn single_dash_long_flags_are_refused() {
    let repo = TestRepo::demo();
    let out = init(repo.path(), &["-force"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--force"));
}
