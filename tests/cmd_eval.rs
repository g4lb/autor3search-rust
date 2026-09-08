mod common;
use common::TestRepo;

/// A fast config: 4 rounds, criterion's minimum sample size, short times.
/// Enough to reach significance on a 2x win without taking minutes.
fn fast_config(repo: &TestRepo) {
    let p = repo.path().join(".autor3search/config.yaml");
    let text = std::fs::read_to_string(&p)
        .unwrap()
        .replace("count: 10", "count: 4")
        .replace("sample_size: 50", "sample_size: 10")
        .replace("measurement_time: 2s", "measurement_time: 500ms")
        .replace("warm_up_time: 1s", "warm_up_time: 200ms");
    std::fs::write(&p, text).unwrap();
}

const FAST_COUNT_WORDS: &str = r#"
    let mut counts = HashMap::new();
    for field in s.split_whitespace() {
        let mut word = String::with_capacity(field.len());
        for c in field.chars() {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                word.push(c);
            }
        }
        if !word.is_empty() {
            *counts.entry(word).or_insert(0) += 1;
        }
    }
    counts
"#;

#[test]
#[ignore = "runs real benchmarks; slow"]
fn a_genuine_optimization_is_kept_and_advances_the_measurement_commit() {
    let repo = TestRepo::demo();
    common::run_cli(&repo, &["init"]);
    fast_config(&repo);
    common::git(repo.path(), &["add", "-A"]);
    common::git(repo.path(), &["commit", "-qm", "init"]);
    common::run_cli(&repo, &["baseline", "--tag", "t1"]);

    let lib = std::fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    let start = lib.find("    let mut counts").unwrap();
    let end = lib.find("\n#[cfg(test)]").unwrap();
    let body_end = lib[..end].rfind("}\n").unwrap();
    let optimized = format!("{}{}{}", &lib[..start], FAST_COUNT_WORDS, &lib[body_end..]);
    std::fs::write(repo.path().join("src/lib.rs"), optimized).unwrap();
    common::git(repo.path(), &["commit", "-qam", "with_capacity + push"]);

    let out = common::run_cli(&repo, &["eval", "--json", "--desc", "with_capacity"]);
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("eval --json must print one object");
    assert_eq!(json["status"], "KEEP", "{json}");
    assert_eq!(out.status.code(), Some(0));

    // The measurement baseline must advance, or a later no-op would coast to
    // KEEP on this win. Located by walking the repo's own state home
    // (`common::state_dir`) rather than calling `state::state_dir` directly:
    // that reads AUTOR3SEARCH_RUST_STATE_HOME from this process's own
    // environment, which `run_cli` only ever sets for the child it spawns.
    let dir = common::state_dir(&repo, "t1");
    let b = autor3search::state::Baseline::load(&dir.join("baseline.json")).unwrap();
    assert_ne!(
        b.measure_commit, b.commit,
        "measure_commit must advance on KEEP"
    );
    assert_eq!(b.commit, json["run"]["baseline_commit"].as_str().unwrap());
}

#[test]
#[ignore = "runs real benchmarks; slow"]
fn a_comment_only_change_is_discarded() {
    let repo = TestRepo::demo();
    common::run_cli(&repo, &["init"]);
    fast_config(&repo);
    common::git(repo.path(), &["add", "-A"]);
    common::git(repo.path(), &["commit", "-qm", "init"]);
    common::run_cli(&repo, &["baseline", "--tag", "t1"]);

    let mut lib = std::fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    lib.push_str("\n// a comment, and nothing else\n");
    std::fs::write(repo.path().join("src/lib.rs"), lib).unwrap();
    common::git(repo.path(), &["commit", "-qam", "comment"]);

    let out = common::run_cli(&repo, &["eval", "--json"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["status"], "DISCARD", "{json}");
    assert_eq!(out.status.code(), Some(1));
}

// --json prints one object and nothing else, by contract: program.md parses it.
#[test]
#[ignore = "runs real benchmarks; slow"]
fn json_output_is_exactly_one_object() {
    let repo = TestRepo::demo();
    common::run_cli(&repo, &["init"]);
    fast_config(&repo);
    common::git(repo.path(), &["add", "-A"]);
    common::git(repo.path(), &["commit", "-qm", "init"]);
    common::run_cli(&repo, &["baseline", "--tag", "t1"]);
    let out = common::run_cli(&repo, &["eval", "--json"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(text.trim().matches("\"status\"").count(), 1, "{text}");
    serde_json::from_str::<serde_json::Value>(text.trim()).expect("valid JSON");
}
