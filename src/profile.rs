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
        let symbols = Symbols::load(&syms_path(&dest));
        let top = top_self_time(&profile_json, &symbols)
            .map_err(|e| format!("parse {}: {e}", dest.display()))?;

        out.push_str(&format!("=== {} ({}) ===\n", t.target, t.package));
        if top.is_empty() {
            out.push_str("  (no samples captured)\n");
        }
        if symbols.is_empty() && !top.is_empty() {
            out.push_str(
                "  (no symbols: this samply cannot presymbolicate, so frames are addresses)\n",
            );
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
    // Recorded with `--unstable-presymbolicate` first. Without it samply
    // leaves every native frame as a bare relative address and defers
    // symbolication to `samply load`, which makes this command's whole
    // output a column of hex. The flag is marked unstable, so a samply that
    // does not have it gets a second, plain attempt rather than an error.
    match record_once(binary, dest, filter, profile_time_secs, true) {
        Ok(()) => Ok(()),
        Err(_) => record_once(binary, dest, filter, profile_time_secs, false),
    }
}

fn record_once(
    binary: &Path,
    dest: &Path,
    filter: &str,
    profile_time_secs: u64,
    presymbolicate: bool,
) -> Result<(), String> {
    let mut cmd = Command::new("samply");
    cmd.args(["record", "--save-only"]);
    if presymbolicate {
        cmd.arg("--unstable-presymbolicate");
    }
    let out = cmd
        .arg("-o")
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

/// Where samply writes the `--unstable-presymbolicate` sidecar for a profile
/// saved at `dest`: the same path with `.json` replaced by `.syms.json`.
fn syms_path(dest: &Path) -> PathBuf {
    dest.with_extension("syms.json")
}

/// Symbol names for the addresses in a profile, read from samply's
/// presymbolication sidecar.
///
/// Missing or malformed is not an error: the profile still parses, its
/// frames just stay as addresses. This is advisory output, and half a
/// report beats refusing to print one.
#[derive(Default)]
pub struct Symbols {
    names: Vec<String>,
    /// Keyed by a library's debug name; each value is that library's
    /// `(start, size, name index)` ranges, sorted by start address.
    libs: HashMap<String, Vec<(u64, u64, usize)>>,
}

#[derive(serde::Deserialize)]
struct SymsSidecar {
    string_table: Vec<String>,
    data: Vec<SymsLib>,
}

#[derive(serde::Deserialize)]
struct SymsLib {
    debug_name: String,
    symbol_table: Vec<SymsEntry>,
}

#[derive(serde::Deserialize)]
struct SymsEntry {
    rva: u64,
    size: u64,
    symbol: usize,
}

impl Symbols {
    /// Parses a sidecar, or returns an empty resolver if it cannot be read.
    pub fn load(path: &Path) -> Symbols {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Symbols::default();
        };
        Symbols::parse(&text).unwrap_or_default()
    }

    fn parse(text: &str) -> Option<Symbols> {
        let sidecar: SymsSidecar = serde_json::from_str(text).ok()?;
        let mut libs: HashMap<String, Vec<(u64, u64, usize)>> = HashMap::new();
        for lib in sidecar.data {
            let mut ranges: Vec<(u64, u64, usize)> = lib
                .symbol_table
                .into_iter()
                .map(|e| (e.rva, e.size, e.symbol))
                .collect();
            ranges.sort_by_key(|(start, _, _)| *start);
            libs.insert(lib.debug_name, ranges);
        }
        Some(Symbols {
            names: sidecar.string_table,
            libs,
        })
    }

    /// The symbol covering `addr` in `lib`, if the sidecar names one.
    ///
    /// The table is a set of `[start, start + size)` ranges, so the
    /// candidate is the last range starting at or before `addr` — and it
    /// only counts when `addr` actually falls inside it. System libraries
    /// samply could not symbolicate simply have no covering range.
    fn resolve(&self, lib: &str, addr: u64) -> Option<&str> {
        let ranges = self.libs.get(lib)?;
        let i = ranges
            .partition_point(|(start, _, _)| *start <= addr)
            .checked_sub(1)?;
        let (start, size, name) = ranges[i];
        if addr >= start.saturating_add(size) {
            return None;
        }
        self.names.get(name).map(String::as_str)
    }

    fn is_empty(&self) -> bool {
        self.libs.is_empty()
    }
}

/// The columns of the Firefox Profiler JSON needed to attribute a sample's
/// self time to a function name. Everything else in the real file (markers,
/// categories, libs, ...) is ignored.
#[derive(serde::Deserialize)]
struct FirefoxProfile {
    threads: Vec<Thread>,
    /// Every shared library in the profile, indexed by `resourceTable.lib`.
    /// Absent in the hand-written profiles the tests use.
    #[serde(default)]
    libs: Vec<Lib>,
}

#[derive(serde::Deserialize)]
struct Lib {
    #[serde(rename = "debugName")]
    debug_name: String,
}

#[derive(serde::Deserialize)]
struct Thread {
    // samply 0.13 renamed this field from "stringTable" to "stringArray".
    // Accepting both means a profile from either era loads, rather than
    // `profile` exiting 2 on a decode error the moment samply moves on.
    #[serde(rename = "stringTable", alias = "stringArray")]
    string_table: Vec<String>,
    #[serde(rename = "funcTable")]
    func_table: FuncTable,
    #[serde(rename = "frameTable")]
    frame_table: FrameTable,
    #[serde(rename = "stackTable")]
    stack_table: StackTable,
    #[serde(rename = "resourceTable", default)]
    resource_table: ResourceTable,
    samples: Samples,
}

impl Thread {
    /// The debug name of the library a function belongs to.
    fn lib_of<'a>(&self, func: usize, libs: &'a [Lib]) -> Option<&'a str> {
        let resource = usize::try_from(*self.func_table.resource.get(func)?).ok()?;
        let lib = (*self.resource_table.lib.get(resource)?)?;
        libs.get(lib).map(|l| l.debug_name.as_str())
    }
}

/// Reads a bare relative address as samply writes it into the string table
/// when a frame is not symbolicated: `0x` followed by hex.
fn parse_address(name: &str) -> Option<u64> {
    u64::from_str_radix(name.strip_prefix("0x")?, 16).ok()
}

#[derive(serde::Deserialize)]
struct FuncTable {
    name: Vec<usize>,
    /// Index into the thread's `resourceTable`, or a negative value for a
    /// function belonging to no library.
    #[serde(default)]
    resource: Vec<i64>,
}

#[derive(serde::Deserialize, Default)]
struct ResourceTable {
    #[serde(default)]
    lib: Vec<Option<usize>>,
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

/// What a frame is called in the report.
///
/// A frame samply already symbolicated keeps its name. One left as a bare
/// address is looked up in the sidecar, and failing that is qualified with
/// its library — `libsystem_malloc.dylib!0x2a15c` says considerably more
/// about where the time went than `0x2a15c` does.
fn label(name: &str, func: usize, thread: &Thread, libs: &[Lib], symbols: &Symbols) -> String {
    let Some(addr) = parse_address(name) else {
        return name.to_string();
    };
    let Some(lib) = thread.lib_of(func, libs) else {
        return name.to_string();
    };
    match symbols.resolve(lib, addr) {
        Some(resolved) => resolved.to_string(),
        None => format!("{lib}!{name}"),
    }
}

/// Aggregates self time (sample count) per function name across every
/// thread, sorted by count descending (ties broken by name, for a
/// deterministic order).
///
/// Self time is what a sample's LEAF frame spent CPU in, so only the leaf
/// needs resolving — walking the full parent chain via `stackTable.prefix`
/// would attribute the same sample to every caller too, which is call-tree
/// time, not self time.
fn top_self_time(json: &str, symbols: &Symbols) -> Result<Vec<(String, usize)>, String> {
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
            *counts
                .entry(label(name, func, thread, &profile.libs, symbols))
                .or_insert(0) += 1;
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
        let top = top_self_time(json, &Symbols::default()).unwrap();
        assert_eq!(top[0], ("hot".to_string(), 2));
        assert!(top.contains(&("warm".to_string(), 1)));
        assert!(top.contains(&("cold".to_string(), 1)));
    }

    // A profile whose frames are bare addresses, plus the sidecar that
    // names them: this is what samply 0.13 actually produces, and without
    // the join the report is a column of hex.
    const ADDRESSED_PROFILE: &str = r#"{
        "libs": [{"debugName": "bench-abc"}, {"debugName": "libsystem_malloc.dylib"}],
        "threads": [
            {
                "stringArray": ["0x1010", "0x9999"],
                "funcTable": {"name": [0, 1], "resource": [0, 1]},
                "resourceTable": {"lib": [0, 1]},
                "frameTable": {"func": [0, 1]},
                "stackTable": {"frame": [0, 1]},
                "samples": {"stack": [0, 0, 1]}
            }
        ]
    }"#;

    const SIDECAR: &str = r#"{
        "string_table": ["UNKNOWN", "demo::count_words"],
        "data": [
            {"debug_name": "bench-abc", "symbol_table": [{"rva": 4096, "size": 64, "symbol": 1}]}
        ]
    }"#;

    #[test]
    fn addresses_are_resolved_through_the_presymbolication_sidecar() {
        let symbols = Symbols::parse(SIDECAR).expect("sidecar parses");
        let top = top_self_time(ADDRESSED_PROFILE, &symbols).unwrap();
        // 0x1010 == 4112, inside [4096, 4160).
        assert_eq!(top[0], ("demo::count_words".to_string(), 2));
    }

    // A system library samply could not symbolicate has no covering range.
    // Naming the library still tells the reader where the time went; a bare
    // "0x9999" tells them nothing.
    #[test]
    fn an_unresolvable_address_is_qualified_with_its_library() {
        let symbols = Symbols::parse(SIDECAR).expect("sidecar parses");
        let top = top_self_time(ADDRESSED_PROFILE, &symbols).unwrap();
        assert!(
            top.contains(&("libsystem_malloc.dylib!0x9999".to_string(), 1)),
            "got {top:?}"
        );
    }

    // An address past the end of the last symbol's range must not be
    // attributed to that symbol.
    #[test]
    fn an_address_beyond_a_symbols_range_does_not_borrow_its_name() {
        let symbols = Symbols::parse(SIDECAR).expect("sidecar parses");
        assert_eq!(
            symbols.resolve("bench-abc", 4159),
            Some("demo::count_words")
        );
        assert_eq!(symbols.resolve("bench-abc", 4160), None);
        assert_eq!(symbols.resolve("bench-abc", 4095), None);
    }

    // Advisory output: no sidecar, or a corrupt one, still reports.
    #[test]
    fn a_missing_or_malformed_sidecar_leaves_frames_as_addresses() {
        for symbols in [
            Symbols::load(Path::new("/nonexistent/profile.syms.json")),
            Symbols::parse("{ not json").unwrap_or_default(),
        ] {
            assert!(symbols.is_empty());
            let top = top_self_time(ADDRESSED_PROFILE, &symbols).unwrap();
            assert!(
                top.contains(&("bench-abc!0x1010".to_string(), 2)),
                "got {top:?}"
            );
        }
    }

    #[test]
    fn the_sidecar_sits_beside_the_profile_it_describes() {
        assert_eq!(
            syms_path(Path::new("/p/demo-wordcount.json")),
            PathBuf::from("/p/demo-wordcount.syms.json")
        );
    }

    #[test]
    fn no_samples_is_an_empty_report_not_an_error() {
        let json = r#"{"threads": [{"stringTable": [], "funcTable": {"name": []}, "frameTable": {"func": []}, "stackTable": {"frame": []}, "samples": {"stack": [null]}}]}"#;
        assert_eq!(
            top_self_time(json, &Symbols::default()).unwrap(),
            Vec::new()
        );
    }

    // samply 0.13.1 — the version `cargo install samply` gives you — renamed
    // the per-thread string table to "stringArray". Profiles written by
    // older samply still use "stringTable", so both names must load; before
    // this, `profile` exited 2 with a decode error against current samply.
    #[test]
    fn a_profile_from_samply_0_13_using_string_array_is_read() {
        let json = r#"{
            "threads": [
                {
                    "stringArray": ["hot", "warm"],
                    "funcTable": {"name": [0, 1]},
                    "frameTable": {"func": [0, 1]},
                    "stackTable": {"frame": [0, 1]},
                    "samples": {"stack": [0, 0, 1]}
                }
            ]
        }"#;
        let top = top_self_time(json, &Symbols::default()).unwrap();
        assert_eq!(top[0], ("hot".to_string(), 2));
        assert!(top.contains(&("warm".to_string(), 1)));
    }

    #[test]
    fn malformed_json_is_reported_not_panicked() {
        assert!(top_self_time("not json", &Symbols::default()).is_err());
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
