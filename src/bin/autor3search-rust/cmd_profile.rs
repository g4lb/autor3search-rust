//! The `profile` command: profiles the declared benchmarks and reports hot
//! spots.
//!
//! **Degrades, never fails, on samply's own absence.** samply is not
//! bundled with Rust and will be missing on most machines; a command that is
//! advisory by nature — nothing downstream depends on its exit code — has no
//! business ending a run over its own absence. The availability check
//! happens before anything else here, including resolving the repository:
//! there is nothing useful to do without it, and no reason to fail loudly
//! for lacking a tool this harness does not require.

use crate::args::Args;
use autor3search::{config, gitx, profile, state};
use std::path::Path;

pub fn run(argv: &[String]) -> i32 {
    let args = match Args::parse(argv, &[("C", true), ("tag", true)]) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return autor3search::EXIT_USAGE;
        }
    };
    if !profile::samply_available() {
        println!(
            "samply is not installed, so there is nothing to profile with.\n\nsamply is chosen \
             over cargo-flamegraph because it needs no elevated privileges — dtrace on macOS \
             requires disabling SIP.\n\nInstall it with:\n\n    {}\n\nthen run \
             'autor3search-rust profile' again.\n",
            profile::INSTALL_HINT
        );
        return 0;
    }
    match run_profile(&args) {
        Ok(msg) => {
            print!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            2
        }
    }
}

/// Resolves which run to profile against, matching `status`'s resolution: an
/// explicit `--tag`, or the tag the current branch encodes.
fn resolve_tag(root: &Path, tag_flag: Option<&str>) -> Result<String, String> {
    if let Some(t) = tag_flag {
        state::valid_tag(t)?;
        return Ok(t.to_string());
    }
    let branch = gitx::current_branch(root)?;
    state::tag_from_branch(&branch).ok_or_else(|| {
        format!(
            "current branch {branch:?} was not created by 'autor3search-rust baseline --tag \
             <tag>' — pass --tag <tag>, or check out the run branch first"
        )
    })
}

fn run_profile(args: &Args) -> Result<String, String> {
    let root = gitx::root(&args.dir())?;
    let tag = resolve_tag(&root, args.value("tag"))?;
    let cfg = config::Config::load(&root.join(config::CONFIG_PATH))?;
    let dir = state::state_dir(&root, &tag)?;

    let o = profile::Options {
        root: root.clone(),
        profiles_dir: dir.join(state::PROFILES_DIR),
        targets: cfg.bench_targets.clone(),
        benchmarks: cfg.benchmarks.clone(),
        timeout: cfg.timeout_duration()?,
        profile_time_secs: profile::DEFAULT_PROFILE_TIME_SECS,
    };
    profile::run(&o)
}
