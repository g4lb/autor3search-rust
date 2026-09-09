//! The `version` command: reports which build of the harness is running.
//!
//! Worth having because a `results.tsv` row is only as reproducible as the
//! binary that produced it: "which version measured this" is otherwise
//! unanswerable from an installed binary. The commit comes from `build.rs`,
//! which records `git describe --always --dirty` at compile time — `dirty`
//! means the tree that was built had uncommitted changes, so the commit
//! alone no longer describes what was built.

use crate::args::Args;

/// `git describe --always --dirty --abbrev=7`, recorded by `build.rs` at
/// compile time. `"unknown"` when the build was not run inside a git
/// checkout (e.g. from a source tarball).
const GIT_COMMIT: &str = env!("AUTOR3SEARCH_GIT_COMMIT");

pub fn run(argv: &[String]) -> i32 {
    let args = match Args::parse(argv, &[]) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return autor3search::EXIT_USAGE;
        }
    };
    let _ = args; // no flags accepted; parsed only to reject unknown ones
    println!(
        "autor3search-rust {} ({GIT_COMMIT})",
        env!("CARGO_PKG_VERSION")
    );
    0
}
