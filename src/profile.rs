//! Profiles the declared benchmarks and reports hot spots.
//!
//! Runs each declared benchmark under **samply**, chosen over
//! `cargo flamegraph` because it needs no elevated privileges:
//! `cargo flamegraph` shells out to `perf` on Linux and `dtrace` on macOS,
//! and dtrace needs SIP disabled or root, which would make the command
//! unusable for most macOS users out of the box.
//!
//! samply is not bundled with Rust and will be missing on most machines.
//! `profile` is advisory by nature — nothing downstream depends on its
//! exit code — so its own absence must never fail a run: see
//! [`samply_available`] and its caller in `cmd_profile`.

use crate::config::BenchTarget;
use crate::measure::bench_filter;
use crate::runner::Runner;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// What a reader is told to run when samply is missing.
pub const INSTALL_HINT: &str = "cargo install samply";

/// How long each benchmark runs under the profiler. Criterion's
/// `--profile-time` runs the benchmark body over and over with its own
/// statistics switched off for exactly this many seconds, so this bounds
/// wall-clock time per target, not measurement precision — nothing here is
/// scored.
pub const DEFAULT_PROFILE_TIME_SECS: u64 = 10;

/// The largest number of self-time hot spots printed per target.
const TOP_N: usize = 15;

/// Whether `samply` is on `PATH` and runnable.
pub fn samply_available() -> bool {
    Command::new("samply")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Configures [`run`].
pub struct Options {
    pub root: PathBuf,
    /// Where samply writes its Firefox Profiler JSON, one file per target.
    pub profiles_dir: PathBuf,
    pub targets: Vec<BenchTarget>,
    pub benchmarks: Vec<String>,
    pub timeout: Duration,
    pub profile_time_secs: u64,
}

/// Profiles every configured bench target and reports the top self-time
/// symbols in each. Assumes the caller has already checked
/// [`samply_available`] — this returns an ordinary error, not a graceful
/// degradation, for every other failure (a build error, a bench target that
/// will not run, a samply crash), because those are real problems worth
/// surfacing rather than samply's mere absence.
pub fn run(o: &Options) -> Result<String, String> {
    if o.targets.is_empty() {
        return Err(
            "no criterion bench targets configured — run 'autor3search-rust init' first"
                .to_string(),
        );
    }
    std::fs::create_dir_all(&o.profiles_dir)
        .map_err(|e| format!("create {}: {e}", o.profiles_dir.display()))?;

    let filter = bench_filter(&o.benchmarks);
    let mut out = String::new();
    for t in &o.targets {
        let binary = find_bench_binary(&o.root, t, o.timeout)?;
        let dest = o
            .profiles_dir
            .join(format!("{}-{}.json", t.package, t.target));
        record(&binary, &dest, &filter, o.profile_time_secs)?;

        let profile_json =
            std::fs::read_to_string(&dest).map_err(|e| format!("read {}: {e}", dest.display()))?;
        let top =
            top_self_time(&profile_json).map_err(|e| format!("parse {}: {e}", dest.display()))?;

        out.push_str(&format!("=== {} ({}) ===\n", t.target, t.package));
        if top.is_empty() {
            out.push_str("  (no samples captured)\n");
        }
        let total: usize = top.iter().map(|(_, n)| n).sum();
        for (i, (name, count)) in top.iter().take(TOP_N).enumerate() {
            let pct = if total > 0 {
                100.0 * *count as f64 / total as f64
            } else {
                0.0
            };
            out.push_str(&format!("  {:2}. {pct:5.1}%  {name}\n", i + 1));
        }
        out.push_str(&format!("full profile: {}\n", dest.display()));
        out.push_str("open with: samply load <path above>\n\n");
    }
    Ok(out)
}

/// Locates the compiled bench binary for `target`, building it first.
///
/// `cargo bench --no-run --message-format=json` builds without running any
/// benchmark and prints one JSON object per build event; the executable
/// path rides on the `compiler-artifact` event for this exact target.
fn find_bench_binary(
    root: &Path,
    target: &BenchTarget,
    timeout: Duration,
) -> Result<PathBuf, String> {
    let runner = Runner::new(root, timeout);
    let out = runner.cargo(
        &[
            "bench",
            "--no-run",
            "--message-format=json",
            "-p",
            &target.package,
            "--bench",
            &target.target,
        ],
        None,
    )?;
    if !out.ok() {
        return Err(format!(
            "build bench target {} (package {}) failed:\n{}",
            target.target,
            target.package,
            out.tail(30)
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if msg.get("reason").and_then(|v| v.as_str()) != Some("compiler-artifact") {
            continue;
        }
        let is_target = msg
            .get("target")
            .and_then(|t| t.get("name"))
            .and_then(|n| n.as_str())
            == Some(target.target.as_str());
        if !is_target {
            continue;
        }
        if let Some(exe) = msg.get("executable").and_then(|v| v.as_str()) {
            return Ok(PathBuf::from(exe));
        }
    }
    Err(format!(
        "cargo did not report a compiled executable for bench target {} (package {}) — is it \
         declared with `harness = false` in Cargo.toml?",
        target.target, target.package
    ))
}

/// Runs the bench binary under `samply record --save-only`, with criterion's
/// own statistics switched off via `--profile-time`.
fn record(binary: &Path, dest: &Path, filter: &str, profile_time_secs: u64) -> Result<(), String> {
    let out = Command::new("samply")
        .args(["record", "--save-only", "-o"])
        .arg(dest)
        .arg("--")
        .arg(binary)
        .args([
            "--bench",
            "--profile-time",
            &profile_time_secs.to_string(),
            filter,
        ])
        .output()
        .map_err(|e| format!("run samply: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "samply record failed (exit {:?}):\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

/// The columns of the Firefox Profiler JSON needed to attribute a sample's
/// self time to a function name. Everything else in the real file (markers,
/// categories, libs, ...) is ignored.
#[derive(serde::Deserialize)]
struct FirefoxProfile {
    threads: Vec<Thread>,
}

#[derive(serde::Deserialize)]
struct Thread {
    #[serde(rename = "stringTable")]
    string_table: Vec<String>,
    #[serde(rename = "funcTable")]
    func_table: FuncTable,
    #[serde(rename = "frameTable")]
    frame_table: FrameTable,
    #[serde(rename = "stackTable")]
    stack_table: StackTable,
    samples: Samples,
}

#[derive(serde::Deserialize)]
struct FuncTable {
    name: Vec<usize>,
}

#[derive(serde::Deserialize)]
struct FrameTable {
    func: Vec<usize>,
}

#[derive(serde::Deserialize)]
struct StackTable {
    frame: Vec<usize>,
}

#[derive(serde::Deserialize)]
struct Samples {
    stack: Vec<Option<usize>>,
}

/// Aggregates self time (sample count) per function name across every
/// thread, sorted by count descending (ties broken by name, for a
/// deterministic order).
///
/// Self time is what a sample's LEAF frame spent CPU in, so only the leaf
/// needs resolving — walking the full parent chain via `stackTable.prefix`
/// would attribute the same sample to every caller too, which is call-tree
/// time, not self time.
fn top_self_time(json: &str) -> Result<Vec<(String, usize)>, String> {
    let profile: FirefoxProfile =
        serde_json::from_str(json).map_err(|e| format!("decode profiler JSON: {e}"))?;
    let mut counts: HashMap<String, usize> = HashMap::new();
    for thread in &profile.threads {
        for stack in thread.samples.stack.iter().flatten() {
            let Some(&frame) = thread.stack_table.frame.get(*stack) else {
                continue;
            };
            let Some(&func) = thread.frame_table.func.get(frame) else {
                continue;
            };
            let Some(&name_idx) = thread.func_table.name.get(func) else {
                continue;
            };
            let Some(name) = thread.string_table.get(name_idx) else {
                continue;
            };
            *counts.entry(name.clone()).or_insert(0) += 1;
        }
    }
    let mut top: Vec<(String, usize)> = counts.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(top)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_self_time_attributes_each_sample_to_its_leaf_function() {
        // Two threads: the first has two stacks whose leaves are "hot" (2
        // samples) and "warm" (1 sample); the second has a single-function
        // thread ("cold") plus one sample with no stack at all, which must
        // be skipped rather than erroring.
        let json = r#"{
            "threads": [
                {
                    "stringTable": ["hot", "warm"],
                    "funcTable": {"name": [0, 1]},
                    "frameTable": {"func": [0, 1]},
                    "stackTable": {"frame": [0, 1]},
                    "samples": {"stack": [0, 0, 1]}
                },
                {
                    "stringTable": ["cold"],
                    "funcTable": {"name": [0]},
                    "frameTable": {"func": [0]},
                    "stackTable": {"frame": [0]},
                    "samples": {"stack": [0, null]}
                }
            ]
        }"#;
        let top = top_self_time(json).unwrap();
        assert_eq!(top[0], ("hot".to_string(), 2));
        assert!(top.contains(&("warm".to_string(), 1)));
        assert!(top.contains(&("cold".to_string(), 1)));
    }

    #[test]
    fn no_samples_is_an_empty_report_not_an_error() {
        let json = r#"{"threads": [{"stringTable": [], "funcTable": {"name": []}, "frameTable": {"func": []}, "stackTable": {"frame": []}, "samples": {"stack": [null]}}]}"#;
        assert_eq!(top_self_time(json).unwrap(), Vec::new());
    }

    #[test]
    fn malformed_json_is_reported_not_panicked() {
        assert!(top_self_time("not json").is_err());
    }

    #[test]
    fn run_without_targets_is_a_clear_error() {
        let o = Options {
            root: PathBuf::from("."),
            profiles_dir: std::env::temp_dir(),
            targets: Vec::new(),
            benchmarks: Vec::new(),
            timeout: Duration::from_secs(1),
            profile_time_secs: DEFAULT_PROFILE_TIME_SECS,
        };
        let err = run(&o).unwrap_err();
        assert!(err.contains("no criterion bench targets"), "{err}");
    }
}
