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

// `stop --force` must be able to abort a real, in-flight eval: not just
// write a request the agent reads later (that is the graceful path, covered
// by tests/cmd_status_stop.rs), but actually end the process, and end it
// with ABORTED/stop_forced rather than a misreported CRASH or FAIL.
//
// Deterministic rather than sleep-based: this waits for eval's OWN pid file
// to appear (a real signal that it has claimed the run and is under way)
// before sending the signal, rather than sleeping a fixed guess and hoping
// eval is still busy. `cargo build`ing criterion's dependency tree from
// scratch in a fresh TestRepo (no cached target/) reliably takes many
// seconds, which is the window `stop --force` has to act inside — this
// test does not race that window, it only needs eval to still be alive
// when the signal arrives, which the pid-file wait guarantees regardless of
// how long the build actually takes.
#[test]
#[ignore = "runs real benchmarks; slow"]
fn stop_force_aborts_an_in_flight_eval_and_writes_no_results_row() {
    let repo = TestRepo::demo();
    common::run_cli(&repo, &["init"]);
    fast_config(&repo);
    common::git(repo.path(), &["add", "-A"]);
    common::git(repo.path(), &["commit", "-qm", "init"]);
    common::run_cli(&repo, &["baseline", "--tag", "t1"]);

    // An in-scope change, so eval gets past the scope gate and reaches the
    // build — the step this test relies on taking a while.
    let mut lib = std::fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    lib.push_str("\n// an experiment in flight when stop --force arrives\n");
    std::fs::write(repo.path().join("src/lib.rs"), lib).unwrap();
    common::git(repo.path(), &["commit", "-qam", "in flight"]);

    let child = common::spawn_cli(&repo, &["eval", "--json", "--desc", "abort-me"]);

    // Wait for eval to have actually claimed the run (state::claim_eval
    // writes this before doing anything slow), rather than sleeping a fixed
    // amount — deterministic regardless of how long setup takes to reach
    // the claim.
    let pid_file = common::state_dir(&repo, "t1").join("eval.pid");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !pid_file.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "eval never claimed the run within 30s"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let stop_out = common::run_cli(&repo, &["stop", "--force"]);
    assert!(
        stop_out.status.success(),
        "stop --force failed: {}",
        String::from_utf8_lossy(&stop_out.stderr)
    );
    let stop_text = String::from_utf8_lossy(&stop_out.stdout);
    assert!(
        stop_text.contains("signalling eval"),
        "stop --force did not find a live eval to signal:\n{stop_text}"
    );

    let output = child.wait_with_output().expect("wait for eval to exit");
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "eval --json must still print exactly one object even when aborted: {e}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(json["status"], "ABORTED", "{json}");
    assert_eq!(json["reason"], "stop_forced", "{json}");

    // Nothing was measured, so nothing is recorded — program.md's contract.
    let rows = autor3search::results::load(&repo.path().join("results.tsv")).unwrap();
    assert!(
        rows.is_empty(),
        "an aborted experiment must not be logged: {rows:?}"
    );
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
