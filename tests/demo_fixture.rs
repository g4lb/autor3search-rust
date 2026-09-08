//! The demo crate is the fixture every integration test measures against, so
//! its shape is asserted here rather than assumed. A change that breaks it
//! would otherwise surface as a confusing failure five tasks away.

use autor3search::discover;
use std::path::PathBuf;

fn demo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/demo")
}

#[test]
fn the_demo_crate_declares_one_criterion_bench_target() {
    let targets = discover::bench_targets(&demo()).expect("cargo metadata on the demo crate");
    assert_eq!(targets.len(), 1, "{targets:?}");
    assert_eq!(targets[0].package, "demo");
    assert_eq!(targets[0].target, "wordcount");
    assert!(targets[0].is_criterion, "must set harness = false");
}

#[test]
fn the_demo_bench_declares_count_words() {
    let src = std::fs::read_to_string(demo().join("benches/wordcount.rs")).unwrap();
    assert_eq!(discover::parse_benchmarks(&src), vec!["count_words"]);
}

#[test]
fn the_demo_crate_has_both_an_integration_test_and_an_inline_test() {
    // The freeze gate has two halves and the fixture must exercise both:
    // tests/ is restored byte for byte, src/ inline tests are hash-and-fail.
    assert!(demo().join("tests/wordcount.rs").exists());
    let lib = std::fs::read_to_string(demo().join("src/lib.rs")).unwrap();
    assert!(
        lib.contains("#[cfg(test)]"),
        "src/lib.rs must carry an inline test module"
    );
    assert!(lib.contains("/// ```"), "src/lib.rs must carry a doctest");
}

#[test]
fn the_demo_crate_builds_and_its_tests_pass() {
    let out = std::process::Command::new("cargo")
        .args(["test", "--quiet"])
        .current_dir(demo())
        .output()
        .expect("cargo test in the demo crate");
    assert!(
        out.status.success(),
        "demo tests must pass:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
