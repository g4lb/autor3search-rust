//! The `doctor` command: checks whether this machine can measure reliably.
//!
//! Always informational: `doctor` always exits 0, whatever it finds.

use crate::args::Args;
use autor3search::{doctor, gitx};

pub fn run(argv: &[String]) -> i32 {
    let args = match Args::parse(argv, &[("C", true)]) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return autor3search::EXIT_USAGE;
        }
    };
    // `doctor` still needs a repository root to check disk space and
    // Cargo.toml against, but a directory it cannot resolve is reported as a
    // finding rather than refused outright — see the fallback below.
    let root = match gitx::root(&args.dir()) {
        Ok(r) => r,
        Err(_) => args.dir(),
    };
    for c in doctor::run(&root) {
        let mark = if c.ok { "ok  " } else { "warn" };
        println!("[{mark}] {:<24} {}", c.name, c.detail);
    }
    // Always 0: doctor is informational, never a gate.
    0
}
