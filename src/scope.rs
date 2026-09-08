//! Decides which files an agent is allowed to modify.

use glob::Pattern;

/// Tests repo-relative paths against a set of glob patterns.
pub struct Matcher {
    patterns: Vec<(String, Pattern)>,
}

impl Matcher {
    /// Compiles patterns such as `src/**`, `crates/*/src/**`, or the Go tool's
    /// `./...` and `./src/...` forms.
    ///
    /// An empty or whitespace-only pattern is skipped rather than treated as
    /// the repository root, so a stray blank entry in a config list matches
    /// nothing instead of silently granting root-level access.
    pub fn new(patterns: &[String]) -> Matcher {
        let mut out = Vec::new();
        for raw in patterns {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let normalized = normalize_pattern(trimmed);
            if let Ok(p) = Pattern::new(&normalized) {
                out.push((normalized, p));
            }
        }
        Matcher { patterns: out }
    }

    /// Reports whether `rel` is inside the allowed scope.
    ///
    /// A path that is absolute, or that climbs out of the root, is never in
    /// scope no matter what the patterns say — and that has to be rejected
    /// explicitly, because it would otherwise be *admitted*: a recursive
    /// pattern matches whatever it is handed, so the one pattern meaning "the
    /// whole repository" would have been the one meaning "anywhere on disk".
    ///
    /// Nothing produces such a path today — callers pass `git diff
    /// --name-only` and `git ls-files` output, which is always root-relative —
    /// so this is the gate refusing to depend on that staying true.
    pub fn matches(&self, rel: &str) -> bool {
        let rel = rel.replace('\\', "/");
        if rel.starts_with('/') || rel.contains(':') {
            return false;
        }
        if rel == ".." || rel.starts_with("../") || rel.split('/').any(|c| c == "..") {
            return false;
        }
        let rel = rel.strip_prefix("./").unwrap_or(&rel);

        for (pattern_str, pattern) in &self.patterns {
            if pattern.matches(rel) {
                // For patterns with "*" but not "**", restrict to immediate children
                if pattern_str.contains('*') && !pattern_str.contains("**") {
                    // Count slashes in the pattern vs the matched path
                    let pattern_depth = pattern_str.matches('/').count();
                    let path_depth = rel.matches('/').count();
                    if path_depth != pattern_depth {
                        continue;
                    }
                }
                return true;
            }
        }
        false
    }
}

/// Rewrites the Go tool's `./...` patterns into globs, so a config copied
/// from the Go tool still means what its author intended.
fn normalize_pattern(p: &str) -> String {
    let p = p.replace('\\', "/");
    let p = p.strip_prefix("./").unwrap_or(&p).to_string();
    if p == "..." {
        return "**".to_string();
    }
    if let Some(prefix) = p.strip_suffix("/...") {
        return format!("{prefix}/**");
    }
    // If the pattern is a simple directory name (no glob chars), match immediate children only
    if !p.contains('*') && !p.contains('?') && !p.contains('[') && !p.ends_with('/') {
        return format!("{p}/*");
    }
    p
}

/// Files whose contents are locked for the life of a run, regardless of scope.
///
/// Returns the reason when `rel` is one, so the gate's message can say what
/// kind of decision is being refused rather than just "no".
///
/// The two `.cargo/config` spellings and the toolchain files are a
/// Rust-specific cheat vector with no Go equivalent. `.cargo/config.toml` can
/// set `RUSTFLAGS` — an agent could add `-C target-cpu=native` and post a
/// real, reproducible speedup having changed no logic whatsoever — and
/// `rust-toolchain.toml` can swap the compiler out from under the
/// measurement. `[profile.release]`'s `opt-level`, `lto` and `codegen-units`
/// are covered by locking `Cargo.toml`.
pub fn locked_file(rel: &str) -> Option<&'static str> {
    let rel = rel.replace('\\', "/");
    let file_name = rel.rsplit('/').next().unwrap_or(&rel);

    if file_name == "Cargo.toml" || file_name == "Cargo.lock" {
        return Some(
            "dependency and build-profile changes are a human decision, not an autonomous one, \
             and would change what is being measured rather than how fast it runs",
        );
    }
    if (file_name == "config" || file_name == "config.toml")
        && rel.rsplit('/').nth(1) == Some(".cargo")
    {
        return Some(
            "it sets compiler flags: an agent could add -C target-cpu=native and post a real \
             speedup having changed no logic at all",
        );
    }
    if rel == "rust-toolchain" || rel == "rust-toolchain.toml" {
        return Some("it selects the toolchain that compiles both sides of the measurement");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(patterns: &[&str]) -> Matcher {
        Matcher::new(&patterns.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn a_recursive_glob_matches_at_every_depth() {
        let m = m(&["src/**"]);
        assert!(m.matches("src/lib.rs"));
        assert!(m.matches("src/deep/nested/mod.rs"));
        assert!(!m.matches("benches/b.rs"));
        assert!(!m.matches("Cargo.toml"));
    }

    #[test]
    fn a_bare_directory_matches_only_its_immediate_children() {
        let m = m(&["src"]);
        assert!(m.matches("src/lib.rs"));
        assert!(!m.matches("src/deep/nested.rs"));
    }

    // Go-style patterns still work, so a config hand-copied from the Go tool
    // does not silently match nothing.
    #[test]
    fn go_style_patterns_are_accepted() {
        assert!(m(&["./..."]).matches("anything/at/all.rs"));
        assert!(m(&["./src/..."]).matches("src/deep/x.rs"));
        assert!(!m(&["./src/..."]).matches("benches/x.rs"));
    }

    #[test]
    fn multiple_patterns_are_a_union() {
        let m = m(&["src/**", "crates/core/**"]);
        assert!(m.matches("src/a.rs"));
        assert!(m.matches("crates/core/b.rs"));
        assert!(!m.matches("crates/other/c.rs"));
    }

    // A blank entry must match nothing rather than silently granting
    // root-level access to everything.
    #[test]
    fn a_blank_pattern_matches_nothing() {
        let m = m(&["   ", ""]);
        assert!(!m.matches("src/lib.rs"));
        assert!(!m.matches("anything"));
    }

    // Nothing produces these today — callers pass git output, which is always
    // root-relative — so this is the gate refusing to depend on that staying
    // true. Without it, "src/**" would be the one pattern meaning "anywhere on
    // the disk".
    #[test]
    fn absolute_and_climbing_paths_are_never_in_scope() {
        let m = m(&["**"]);
        assert!(!m.matches("/etc/passwd"));
        assert!(!m.matches("../outside.rs"));
        assert!(!m.matches(".."));
        if cfg!(windows) {
            assert!(!m.matches("C:\\evil.rs"));
        }
    }

    #[test]
    fn backslash_separators_are_normalized() {
        assert!(m(&["src/**"]).matches("src\\deep\\x.rs"));
    }

    #[test]
    fn cargo_manifests_are_locked_at_any_depth() {
        assert!(locked_file("Cargo.toml").is_some());
        assert!(locked_file("Cargo.lock").is_some());
        assert!(locked_file("crates/core/Cargo.toml").is_some());
        assert!(locked_file("src/lib.rs").is_none());
    }

    // The Rust-specific cheat vector: RUSTFLAGS in .cargo/config can post a
    // real, reproducible speedup with no logic change at all.
    #[test]
    fn cargo_config_is_locked_in_both_spellings_and_at_any_depth() {
        assert!(locked_file(".cargo/config.toml").is_some());
        assert!(locked_file(".cargo/config").is_some());
        assert!(locked_file("crates/core/.cargo/config.toml").is_some());
    }

    #[test]
    fn toolchain_files_are_locked() {
        assert!(locked_file("rust-toolchain.toml").is_some());
        assert!(locked_file("rust-toolchain").is_some());
    }

    #[test]
    fn locked_reasons_name_the_kind_of_decision_being_refused() {
        assert!(locked_file("Cargo.toml").unwrap().contains("dependency"));
        assert!(
            locked_file(".cargo/config.toml")
                .unwrap()
                .contains("compiler flags")
        );
        assert!(
            locked_file("rust-toolchain.toml")
                .unwrap()
                .contains("toolchain")
        );
    }
}
