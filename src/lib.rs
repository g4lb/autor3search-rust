//! autor3search-rust is a frozen measurement harness that lets an AI coding
//! agent autonomously optimize a Rust repository.
//!
//! The library never edits source code. It gates correctness, measures a
//! candidate against a pinned baseline, and returns a verdict.

/// Exit code for a usage error. Verdict exit codes live in [`verdict`].
pub const EXIT_USAGE: i32 = 64;

pub mod bench;
pub mod config;
pub mod discover;
pub mod doctor;
pub mod freeze;
pub mod gitx;
pub mod measure;
pub mod pipeline;
pub mod profile;
pub mod results;
pub mod runner;
pub mod scope;
pub mod state;
pub mod stats;
pub mod verdict;
