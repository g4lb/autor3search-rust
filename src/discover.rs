//! Finds benchmarks, bench targets and test files in a Rust repository.

use crate::scope::Matcher;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// One cargo bench target, package-qualified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BenchTargetInfo {
    pub package: String,
    pub target: String,
    pub src_path: PathBuf,
    /// Whether it declares `harness = false`, i.e. is a criterion target.
    /// Ordinary libtest targets do not accept criterion's flags, so
    /// measurement must never invoke them.
    pub is_criterion: bool,
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<MetaPackage>,
    workspace_members: Vec<String>,
}

#[derive(Deserialize)]
struct MetaPackage {
    id: String,
    name: String,
    targets: Vec<MetaTarget>,
}

#[derive(Deserialize)]
struct MetaTarget {
    name: String,
    kind: Vec<String>,
    src_path: String,
    #[serde(default)]
    harness: Option<bool>,
}

/// Lists every bench target in the workspace via `cargo metadata`.
///
/// `--no-deps` keeps this to the workspace's own packages, and metadata is
/// produced without compiling anything, so this works on a tree whose
/// benchmarks do not currently build.
pub fn bench_targets(root: &Path) -> Result<Vec<BenchTargetInfo>, String> {
    let out = std::process::Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("run cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let meta: Metadata =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("parse cargo metadata: {e}"))?;

    let mut found = Vec::new();
    for pkg in &meta.packages {
        if !meta.workspace_members.contains(&pkg.id) {
            continue;
        }
        for t in &pkg.targets {
            if !t.kind.iter().any(|k| k == "bench") {
                continue;
            }
            found.push(BenchTargetInfo {
                package: pkg.name.clone(),
                target: t.name.clone(),
                src_path: PathBuf::from(&t.src_path),
                // cargo reports harness: false explicitly; the default is true.
                is_criterion: t.harness == Some(false),
            });
        }
    }
    found.sort_by(|a, b| (&a.package, &a.target).cmp(&(&b.package, &b.target)));
    Ok(found)
}

/// Extracts benchmark names from criterion's `--list` output.
///
/// Criterion prints one `<name>: benchmark` line per benchmark, mixed in with
/// cargo's own build chatter and a possible gnuplot notice, so the suffix is
/// what identifies a real line.
pub fn parse_list_output(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.trim_end().strip_suffix(": benchmark"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Best-effort benchmark names parsed straight from a bench source file.
///
/// Used only when `--list` cannot run because the benchmarks do not compile.
/// It finds string-literal arguments to `bench_function` and
/// `bench_with_input`; a name computed at runtime cannot be seen this way,
/// which is why `init` says the list is best-effort when it falls back here.
pub fn parse_benchmarks(source: &str) -> Vec<String> {
    let Ok(file) = syn::parse_file(source) else {
        return Vec::new();
    };
    struct Visitor {
        names: Vec<String>,
    }
    impl<'ast> syn::visit::Visit<'ast> for Visitor {
        fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
            let method = node.method.to_string();
            if method == "bench_function" || method == "bench_with_input" {
                if let Some(syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                })) = node.args.first()
                {
                    let name = s.value();
                    if !self.names.contains(&name) {
                        self.names.push(name);
                    }
                }
            }
            syn::visit::visit_expr_method_call(self, node);
        }
    }
    let mut v = Visitor { names: Vec::new() };
    syn::visit::Visit::visit_file(&mut v, &file);
    v.names
}

/// Directories never walked: build output, version control, and the harness's
/// own in-repo directory.
fn skip_dir(name: &str) -> bool {
    name == "target" || name.starts_with('.')
}

fn walk_rs(root: &Path, rel: &Path, out: &mut Vec<String>) -> Result<(), String> {
    let dir = root.join(rel);
    let entries =
        std::fs::read_dir(&dir).map_err(|e| format!("read dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read dir {}: {e}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let child = rel.join(&name);
        let file_type = entry.file_type().map_err(|e| format!("stat {name}: {e}"))?;
        if file_type.is_dir() {
            if skip_dir(&name) {
                continue;
            }
            walk_rs(root, &child, out)?;
        } else if file_type.is_file() && name.ends_with(".rs") {
            out.push(child.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// Every repo-relative `.rs` file under `tests/` and `benches/`, minus
/// `exclude`, sorted. These are the tests that live in their own files and so
/// can be frozen and restored byte for byte.
pub fn frozen_test_files(root: &Path, exclude: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for top in ["benches", "tests"] {
        if root.join(top).is_dir() {
            walk_rs(root, Path::new(top), &mut out)?;
        }
    }
    out.retain(|p| !exclude.iter().any(|e| e.replace('\\', "/") == *p));
    out.sort();
    Ok(out)
}

/// Every repo-relative `.rs` file the scope allows the agent to edit, sorted.
/// These are the files whose inline `#[cfg(test)]` modules and doctests are
/// hashed at baseline.
pub fn in_scope_sources(root: &Path, m: &Matcher) -> Result<Vec<String>, String> {
    let mut all = Vec::new();
    walk_rs(root, Path::new(""), &mut all)?;
    all.retain(|p| m.matches(p));
    all.sort();
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // Real `cargo bench -- --list` output, captured during design. The
    // trailing ": benchmark" is criterion's own format.
    const LIST_OUTPUT: &str = "\
    Finished `bench` profile [optimized] target(s) in 0.03s
     Running benches/wordcount.rs (target/release/deps/wordcount-9df89d2c)
Gnuplot not found, using plotters backend
count_words: benchmark
parse/small: benchmark
parse/large: benchmark
";

    #[test]
    fn list_output_yields_only_benchmark_names() {
        assert_eq!(
            parse_list_output(LIST_OUTPUT),
            vec!["count_words", "parse/small", "parse/large"]
        );
    }

    #[test]
    fn list_output_with_no_benchmarks_yields_nothing() {
        assert!(parse_list_output("Gnuplot not found, using plotters backend\n").is_empty());
    }

    const BENCH_SRC: &str = r#"
use criterion::{criterion_group, criterion_main, Criterion};

fn benches(c: &mut Criterion) {
    c.bench_function("count_words", |b| b.iter(|| ()));
    c.bench_with_input("parse", &7, |b, i| b.iter(|| *i));
    let mut g = c.benchmark_group("group");
    g.bench_function("inner", |b| b.iter(|| ()));
}
criterion_group!(all, benches);
criterion_main!(all);
"#;

    #[test]
    fn the_syn_fallback_finds_string_literal_benchmark_names() {
        let names = parse_benchmarks(BENCH_SRC);
        assert!(names.contains(&"count_words".to_string()), "{names:?}");
        assert!(names.contains(&"parse".to_string()), "{names:?}");
        assert!(names.contains(&"inner".to_string()), "{names:?}");
    }

    #[test]
    fn the_syn_fallback_survives_unparseable_source() {
        assert!(parse_benchmarks("fn broken( {").is_empty());
    }

    struct Repo {
        _dir: tempfile::TempDir,
        root: std::path::PathBuf,
    }

    fn repo() -> Repo {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for d in ["src", "tests", "benches", "target/debug", "src/deep"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        fs::write(root.join("src/deep/m.rs"), "pub fn g() {}\n").unwrap();
        fs::write(root.join("tests/it.rs"), "#[test] fn t() {}\n").unwrap();
        fs::write(root.join("benches/b.rs"), BENCH_SRC).unwrap();
        fs::write(root.join("target/debug/generated.rs"), "pub fn x() {}\n").unwrap();
        fs::write(root.join("README.md"), "not rust\n").unwrap();
        Repo { _dir: dir, root }
    }

    #[test]
    fn frozen_test_files_covers_tests_and_benches_only() {
        assert_eq!(
            frozen_test_files(&repo().root, &[]).unwrap(),
            vec!["benches/b.rs", "tests/it.rs"]
        );
    }

    #[test]
    fn frozen_test_files_honours_the_unfreeze_list() {
        let r = repo();
        assert_eq!(
            frozen_test_files(&r.root, &["tests/it.rs".to_string()]).unwrap(),
            vec!["benches/b.rs"]
        );
    }

    // target/ holds build output, not source; freezing or scoping it would be
    // meaningless and enormous.
    #[test]
    fn target_and_dotfiles_are_skipped() {
        let r = repo();
        let m = crate::scope::Matcher::new(&["**".to_string()]);
        let sources = in_scope_sources(&r.root, &m).unwrap();
        assert!(
            !sources.iter().any(|s| s.starts_with("target/")),
            "{sources:?}"
        );
    }

    #[test]
    fn in_scope_sources_returns_only_rust_files_under_scope() {
        let r = repo();
        let m = crate::scope::Matcher::new(&["src/**".to_string()]);
        assert_eq!(
            in_scope_sources(&r.root, &m).unwrap(),
            vec!["src/deep/m.rs", "src/lib.rs"]
        );
    }
}
