//! Finds benchmarks, bench targets and test files in a Rust repository.

use crate::scope::Matcher;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One cargo bench target, package-qualified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BenchTargetInfo {
    pub package: String,
    pub target: String,
    pub src_path: PathBuf,
    /// Whether it declares `harness = false`, i.e. is a criterion target.
    /// This is read from the `[[bench]]` table in the package's Cargo.toml.
    /// An ordinary libtest targets have `harness = true` (the default) and do not
    /// accept criterion's flags, so measurement must never invoke them.
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
    manifest_path: String,
}

#[derive(Deserialize)]
struct MetaTarget {
    name: String,
    kind: Vec<String>,
    src_path: String,
}

#[derive(Deserialize)]
struct CargoManifest {
    bench: Option<Vec<BenchEntry>>,
}

#[derive(Deserialize)]
struct BenchEntry {
    name: String,
    #[serde(default)]
    harness: Option<bool>,
}

/// Determine if a bench target is a criterion target by reading its Cargo.toml.
/// Returns true if the target has `harness = false`, false otherwise.
/// If the manifest cannot be read or parsed, returns false and logs via comment why.
fn is_criterion_target(manifest_path: &str, target_name: &str) -> bool {
    // Try to read and parse the Cargo.toml file
    let manifest_content = match std::fs::read_to_string(manifest_path) {
        Ok(content) => content,
        Err(_) => {
            // If we cannot read the manifest, treat as not criterion to avoid aborting
            // discovery. The measurement phase will fail gracefully if this assumption
            // is wrong, so silently assuming is acceptable here.
            return false;
        }
    };

    let manifest: CargoManifest = match toml::from_str(&manifest_content) {
        Ok(m) => m,
        Err(_) => {
            // If we cannot parse the manifest, treat as not criterion.
            // Again, discovery should not abort on unparseable manifests.
            return false;
        }
    };

    // Look for a [[bench]] entry with matching name
    if let Some(benches) = manifest.bench {
        for bench in benches {
            if bench.name == target_name {
                // Found explicit [[bench]] entry; check harness field
                // Default is harness = true (not criterion)
                return bench.harness == Some(false);
            }
        }
    }

    // No explicit [[bench]] entry found. This bench target was auto-discovered
    // from benches/*.rs by cargo, which defaults to harness = true (libtest).
    false
}

/// Runs `cargo metadata --no-deps` and parses it. Shared by [`bench_targets`]
/// and [`workspace_manifest_paths`] so there is one place that knows how to
/// invoke it.
///
/// `--no-deps` keeps this to the workspace's own packages, and metadata is
/// produced without compiling anything, so this works on a tree whose
/// benchmarks do not currently build.
fn cargo_metadata(root: &Path) -> Result<Metadata, String> {
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
    serde_json::from_slice(&out.stdout).map_err(|e| format!("parse cargo metadata: {e}"))
}

/// Every workspace member's manifest path, as `cargo metadata` reports it.
///
/// Used by [`locked_files`] to stat each member's `Cargo.toml` directly,
/// belt-and-braces alongside the generic filesystem walk: a member reached
/// only through a symlinked directory is a member `cargo` itself will happily
/// build from, so the locked-file check must see its manifest too.
fn workspace_manifest_paths(root: &Path) -> Result<Vec<PathBuf>, String> {
    let meta = cargo_metadata(root)?;
    Ok(meta
        .packages
        .iter()
        .filter(|p| meta.workspace_members.contains(&p.id))
        .map(|p| PathBuf::from(&p.manifest_path))
        .collect())
}

/// Lists every bench target in the workspace via `cargo metadata`.
pub fn bench_targets(root: &Path) -> Result<Vec<BenchTargetInfo>, String> {
    let meta = cargo_metadata(root)?;

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
                is_criterion: is_criterion_target(&pkg.manifest_path, &t.name),
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

/// Directories `locked_files` never walks into: build output and the VCS
/// metadata directory. Deliberately narrower than [`skip_dir`]'s "any
/// dot-prefixed directory" — a locked file can itself live under a
/// dot-prefixed directory (`.cargo/config.toml`), so that rule would hide
/// the very thing this function exists to find.
fn skip_dir_for_locked(name: &str) -> bool {
    name == "target" || name == ".git"
}

/// How deep [`walk_locked`] will recurse before giving up. A backstop for
/// when the cycle guard below cannot help — e.g. `canonicalize` failing on a
/// component it cannot resolve (permission denied, a dangling link in the
/// middle of the chain) — not a limit any real repository should approach.
const MAX_LOCKED_WALK_DEPTH: usize = 64;

/// Recurses into `rel`, recording every locked file found.
///
/// For an ordinary (non-symlink) entry, `DirEntry::file_type()` — filled in
/// from the directory listing itself, with no extra syscall on the platforms
/// this runs on — is trusted directly to decide file vs. directory: with no
/// indirection involved, it reports exactly what `std::fs::metadata` would.
/// Only a symlink entry is ambiguous: `file_type()` reports "symlink" and,
/// by contract, does NOT say what the *target* is, so `metadata` (which
/// follows the link) is called just for that one entry to resolve it — a
/// symlinked `.cargo` is still walked into like a real directory and a
/// symlinked `config.toml` is still recorded like a real file, matching what
/// `cargo` itself sees when it reads these paths at build time. This is what
/// let a symlinked `.cargo` slip past an earlier version of this walk that
/// used `file_type()` alone and treated "symlink" as neither file nor
/// directory: the fix is to fall back to `metadata` for a symlink, not to
/// call it for every entry — which is what made this walk cost one stat
/// syscall per file in the repository regardless of whether anything here
/// could possibly be a locked file. The locked shapes this is actually
/// looking for (`Cargo.toml`/`Cargo.lock` at any depth, `.cargo/config` at
/// any depth) are a handful of fixed names; deciding directory-vs-file for
/// the other, non-symlink entries is the only per-entry cost this needs to
/// pay, and readdir already gives it that for free.
///
/// Following links makes a cycle possible (a symlink pointing at an
/// ancestor, or at another symlink that loops back), so `seen_dirs` records
/// the canonicalized form of every directory entered and refuses to enter one
/// twice; `depth` is a backstop for the rarer case where canonicalization
/// itself cannot be trusted to catch it.
fn walk_locked(
    root: &Path,
    rel: &Path,
    out: &mut Vec<String>,
    seen_dirs: &mut HashSet<PathBuf>,
    depth: usize,
) -> Result<(), String> {
    if depth > MAX_LOCKED_WALK_DEPTH {
        return Err(format!(
            "locked-file walk is over {MAX_LOCKED_WALK_DEPTH} levels deep at {} — a symlink \
             cycle?",
            rel.display()
        ));
    }
    let dir = root.join(rel);
    // A directory reached a second time via a different path — most likely a
    // symlink looping back on an ancestor — is not walked again.
    if let Ok(canon) = dir.canonicalize() {
        if !seen_dirs.insert(canon) {
            return Ok(());
        }
    }
    let entries =
        std::fs::read_dir(&dir).map_err(|e| format!("read dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read dir {}: {e}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let child = rel.join(&name);
        let file_type = entry
            .file_type()
            .map_err(|e| format!("file type {}: {e}", child.display()))?;

        // Only a symlink needs the expensive, link-following stat: for
        // anything else, file_type() already tells the truth.
        let (is_dir, is_file) = if file_type.is_symlink() {
            match std::fs::metadata(root.join(&child)) {
                Ok(meta) => (meta.is_dir(), meta.is_file()),
                // A dangling symlink, or something removed between the
                // readdir and this stat: neither is a locked file to record.
                Err(_) => continue,
            }
        } else {
            (file_type.is_dir(), file_type.is_file())
        };

        if is_dir {
            if skip_dir_for_locked(&name) {
                continue;
            }
            walk_locked(root, &child, out, seen_dirs, depth + 1)?;
        } else if is_file {
            let rel_str = child.to_string_lossy().replace('\\', "/");
            if crate::scope::locked_file(&rel_str).is_some() {
                out.push(rel_str);
            }
        }
    }
    Ok(())
}

/// The fixed, well-known locked locations, checked directly rather than
/// found by traversal: the root's own `.cargo/config[.toml]` and
/// `rust-toolchain[.toml]`, plus `Cargo.toml`/`Cargo.lock` at the root and at
/// every workspace member's manifest path (from `cargo metadata`, which
/// resolves paths through symlinks itself).
///
/// Belt and braces alongside [`walk_locked`]: the walk is generic and finds
/// anything [`crate::scope::locked_file`] recognises anywhere on disk, but
/// this list is small, fixed, and does not depend on the walk's traversal
/// logic — including its symlink handling — being right. `cargo metadata`
/// failing (no workspace here, or an unparseable manifest — itself possibly
/// the tampering being gated against) is not fatal to this check: the fixed
/// root-level paths are still worth statting, so a metadata failure just
/// means the per-member manifests are not added.
fn known_locked_candidates(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![
        root.join(".cargo/config.toml"),
        root.join(".cargo/config"),
        root.join("rust-toolchain"),
        root.join("rust-toolchain.toml"),
        root.join("Cargo.toml"),
        root.join("Cargo.lock"),
    ];
    if let Ok(members) = workspace_manifest_paths(root) {
        out.extend(members);
    }
    out
}

/// Every locked file (see [`crate::scope::locked_file`]) actually present on
/// disk, at any depth, sorted — found by statting the filesystem directly
/// rather than by trusting git's view of what changed.
///
/// `git diff`/`git status` both omit gitignored paths, so a repository that
/// gitignores `Cargo.lock` (common) or `.cargo/` (an agent could add this to
/// an in-scope `.gitignore` itself) makes the git-derived half of the locked-
/// file gate blind to exactly the files it exists to protect. This walk is
/// what lets the gate compare hashes of what is really on disk, independent
/// of whatever git has been told to ignore.
pub fn locked_files(root: &Path) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut seen_dirs = HashSet::new();
    walk_locked(root, Path::new(""), &mut out, &mut seen_dirs, 0)?;

    for path in known_locked_candidates(root) {
        if std::fs::metadata(&path).is_err() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if crate::scope::locked_file(&rel_str).is_some() {
            out.push(rel_str);
        }
    }

    out.sort();
    out.dedup();
    Ok(out)
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
        for d in [
            "src",
            "tests",
            "benches",
            "target/debug",
            "src/deep",
            ".dotdir",
        ] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        fs::write(root.join("src/deep/m.rs"), "pub fn g() {}\n").unwrap();
        fs::write(root.join("tests/it.rs"), "#[test] fn t() {}\n").unwrap();
        fs::write(root.join("benches/b.rs"), BENCH_SRC).unwrap();
        fs::write(root.join("target/debug/generated.rs"), "pub fn x() {}\n").unwrap();
        fs::write(root.join(".dotdir/hidden.rs"), "pub fn z() {}\n").unwrap();
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
        assert!(
            !sources.iter().any(|s| s.starts_with(".dotdir/")),
            "{sources:?}"
        );

        let frozen = frozen_test_files(&r.root, &[]).unwrap();
        assert!(
            !frozen.iter().any(|s| s.starts_with(".dotdir/")),
            "{frozen:?}"
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

    #[test]
    fn bench_targets_workspace_with_same_name_in_multiple_members() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Create a workspace with two members, both declaring a bench target
        // with the same name but different harness settings
        let workspace_toml = r#"
[workspace]
members = ["member1", "member2"]
"#;
        fs::write(root.join("Cargo.toml"), workspace_toml).unwrap();

        // Create member1: criterion bench
        fs::create_dir(root.join("member1")).unwrap();
        let member1_cargo = r#"
[package]
name = "member1"
version = "0.1.0"
edition = "2021"

[[bench]]
name = "shared_bench"
harness = false
"#;
        fs::write(root.join("member1/Cargo.toml"), member1_cargo).unwrap();
        fs::create_dir(root.join("member1/src")).unwrap();
        fs::write(root.join("member1/src/lib.rs"), "pub fn f() {}").unwrap();
        fs::create_dir(root.join("member1/benches")).unwrap();
        fs::write(root.join("member1/benches/shared_bench.rs"), "fn main() {}").unwrap();

        // Create member2: libtest bench (default)
        fs::create_dir(root.join("member2")).unwrap();
        let member2_cargo = r#"
[package]
name = "member2"
version = "0.1.0"
edition = "2021"

[[bench]]
name = "shared_bench"
"#;
        fs::write(root.join("member2/Cargo.toml"), member2_cargo).unwrap();
        fs::create_dir(root.join("member2/src")).unwrap();
        fs::write(root.join("member2/src/lib.rs"), "pub fn g() {}").unwrap();
        fs::create_dir(root.join("member2/benches")).unwrap();
        fs::write(root.join("member2/benches/shared_bench.rs"), "fn main() {}").unwrap();

        // Call bench_targets on the workspace root
        let targets = bench_targets(root).unwrap();

        // Should return both targets, sorted by (package, target)
        assert_eq!(
            targets.len(),
            2,
            "both members' bench targets should be found"
        );

        // Verify first target (member1, should be criterion)
        assert_eq!(targets[0].package, "member1");
        assert_eq!(targets[0].target, "shared_bench");
        assert!(
            targets[0].is_criterion,
            "member1's bench should be criterion"
        );

        // Verify second target (member2, should be libtest)
        assert_eq!(targets[1].package, "member2");
        assert_eq!(targets[1].target, "shared_bench");
        assert!(
            !targets[1].is_criterion,
            "member2's bench should be libtest (default harness = true)"
        );
    }

    #[test]
    fn bench_targets_distinguishes_criterion_from_libtest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Create a minimal Cargo.toml with both harness = false and harness = true benches
        let cargo_toml = r#"
[package]
name = "test-crate"
version = "0.1.0"
edition = "2021"

[[bench]]
name = "criterion_bench"
harness = false

[[bench]]
name = "libtest_bench"
harness = true
"#;
        fs::write(root.join("Cargo.toml"), cargo_toml).unwrap();

        // Create benches directory with source files
        fs::create_dir(root.join("benches")).unwrap();
        fs::write(
            root.join("benches/criterion_bench.rs"),
            "fn main() { println!(\"criterion\"); }",
        )
        .unwrap();
        fs::write(
            root.join("benches/libtest_bench.rs"),
            "fn main() { println!(\"libtest\"); }",
        )
        .unwrap();

        // Test that is_criterion_target correctly reads from Cargo.toml
        assert!(
            is_criterion_target(root.join("Cargo.toml").to_str().unwrap(), "criterion_bench"),
            "harness = false should be criterion"
        );
        assert!(
            !is_criterion_target(root.join("Cargo.toml").to_str().unwrap(), "libtest_bench"),
            "harness = true should not be criterion"
        );
    }

    #[test]
    fn bench_targets_with_no_explicit_entry_defaults_to_libtest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Create a Cargo.toml with NO [[bench]] entries
        let cargo_toml = r#"
[package]
name = "test-crate"
version = "0.1.0"
edition = "2021"
"#;
        fs::write(root.join("Cargo.toml"), cargo_toml).unwrap();

        // An auto-discovered bench target (no [[bench]] entry) defaults to libtest
        assert!(
            !is_criterion_target(
                root.join("Cargo.toml").to_str().unwrap(),
                "any_auto_discovered_bench"
            ),
            "auto-discovered bench should default to libtest (harness = true)"
        );
    }

    #[test]
    fn bench_targets_gracefully_handles_missing_manifest() {
        // Non-existent path should not panic, just return false
        let result = is_criterion_target("/nonexistent/Cargo.toml", "some_bench");
        assert!(
            !result,
            "missing manifest should be treated as not criterion"
        );
    }

    // I2: the direct-stat locked-file walk must find a `.cargo/config.toml`
    // even though `.cargo` is a dot-prefixed directory that the `.rs`-file
    // walk (`skip_dir`) always skips — a locked file can legitimately live
    // under one, and this walk exists precisely to see it regardless of
    // what git has been told to ignore.
    #[test]
    fn locked_files_finds_cargo_config_under_a_dot_directory() {
        let r = repo();
        fs::create_dir_all(r.root.join(".cargo")).unwrap();
        fs::write(r.root.join(".cargo/config.toml"), "[build]\n").unwrap();
        fs::write(r.root.join("Cargo.toml"), "[package]\n").unwrap();
        let found = locked_files(&r.root).unwrap();
        assert_eq!(found, vec![".cargo/config.toml", "Cargo.toml"]);
    }

    // target/ and .git/ hold build output and VCS internals respectively,
    // never something worth hashing, and .git/ in particular can be huge.
    #[test]
    fn locked_files_skips_target_and_git() {
        let r = repo();
        fs::create_dir_all(r.root.join("target/debug")).unwrap();
        fs::write(r.root.join("target/debug/Cargo.toml"), "decoy\n").unwrap();
        fs::create_dir_all(r.root.join(".git/refs")).unwrap();
        fs::write(r.root.join(".git/refs/Cargo.toml"), "decoy\n").unwrap();
        assert!(locked_files(&r.root).unwrap().is_empty());
    }

    // The security-review finding this walk was rewritten for: `DirEntry::
    // file_type()` does NOT follow a symlink, so a symlinked `.cargo` used
    // to look like neither a file nor a directory and the walk silently
    // skipped it — exactly what let `.cargo/config.toml`'s `rustflags`
    // through unnoticed. `metadata` (which does follow links) must walk
    // into it like an ordinary directory and record the config inside.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_locked_directory_is_walked_into_not_skipped() {
        let r = repo();
        let elsewhere = r.root.parent().unwrap().join("elsewhere_cargo");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("config.toml"), "[build]\n").unwrap();
        std::os::unix::fs::symlink(&elsewhere, r.root.join(".cargo")).unwrap();
        assert_eq!(locked_files(&r.root).unwrap(), vec![".cargo/config.toml"]);
    }

    // The locked file itself, not just its containing directory, can be the
    // link.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_locked_file_is_recorded_like_a_real_one() {
        let r = repo();
        let elsewhere = r.root.parent().unwrap().join("elsewhere_config.toml");
        fs::write(&elsewhere, "[build]\n").unwrap();
        fs::create_dir_all(r.root.join(".cargo")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, r.root.join(".cargo/config.toml")).unwrap();
        assert_eq!(locked_files(&r.root).unwrap(), vec![".cargo/config.toml"]);
    }

    // Following links makes a cycle possible: a symlink pointing back at an
    // ancestor of the walk must not recurse forever.
    #[cfg(unix)]
    #[test]
    fn a_symlink_cycle_does_not_hang_the_walk() {
        let r = repo();
        fs::create_dir_all(r.root.join("loop")).unwrap();
        // "loop/back" points at "loop" itself, one level up from where it
        // sits — an unbounded walk would recurse into "loop/back/back/..."
        // forever.
        std::os::unix::fs::symlink(r.root.join("loop"), r.root.join("loop/back")).unwrap();
        fs::write(r.root.join("Cargo.toml"), "[package]\n").unwrap();
        // Must terminate, and must still find the one real locked file.
        assert_eq!(locked_files(&r.root).unwrap(), vec!["Cargo.toml"]);
    }

    // A repository legitimately reached through a symlinked ancestor (macOS's
    // `/tmp`, a home directory on a linked volume) is not tampering: the walk
    // takes `root` on faith and only examines what is beneath it, so a link
    // ABOVE `root` must have no effect on what is found.
    #[cfg(unix)]
    #[test]
    fn locked_files_is_unaffected_by_a_symlinked_ancestor_of_root() {
        let outer = tempfile::tempdir().unwrap();
        let real = outer.path().join("real");
        fs::create_dir_all(&real).unwrap();
        let link = outer.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let root = link.join("repo");
        fs::create_dir_all(root.join(".cargo")).unwrap();
        fs::write(root.join(".cargo/config.toml"), "[build]\n").unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(
            locked_files(&root).unwrap(),
            vec![".cargo/config.toml", "Cargo.toml"]
        );
    }

    #[test]
    fn bench_targets_gracefully_handles_unparseable_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Create an invalid TOML file
        fs::write(root.join("Cargo.toml"), "invalid [[ toml").unwrap();

        // Unparseable manifest should not panic, just return false
        let result = is_criterion_target(root.join("Cargo.toml").to_str().unwrap(), "some_bench");
        assert!(
            !result,
            "unparseable manifest should be treated as not criterion"
        );
    }
}
