//! Loads and validates the autor3search-rust run configuration.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// The config location relative to the repository root.
pub const CONFIG_PATH: &str = ".autor3search/config.yaml";

/// The smallest `count` at which the exact Mann-Whitney test used by
/// [`crate::stats`] can ever report p < 0.05, however large or clean the
/// improvement. At 2 and 3 rounds per side the best achievable two-sided
/// p-value is 0.3333 and 0.1 — both above the default alpha — so every
/// experiment would discard with nothing in the output explaining why.
pub const MIN_COUNT: usize = 4;

/// Criterion refuses a sample size below this.
pub const MIN_SAMPLE_SIZE: usize = 10;

/// One criterion bench target, package-qualified.
///
/// A bare target name is ambiguous the moment two workspace members declare a
/// bench target of the same name, so the package always travels with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchTarget {
    pub package: String,
    pub target: String,
}

/// Controls what is measured, what may be edited, and how strictly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The declared benchmark set. Empty means every discovered benchmark.
    #[serde(default)]
    pub benchmarks: Vec<String>,
    /// The criterion bench targets declaring them.
    #[serde(default)]
    pub bench_targets: Vec<BenchTarget>,
    /// Glob patterns for the paths the agent may modify.
    #[serde(default = "default_scope")]
    pub scope: Vec<String>,
    /// Measured rounds per side.
    #[serde(default = "default_count")]
    pub count: usize,
    /// Passed to criterion as `--sample-size`.
    #[serde(default = "default_sample_size")]
    pub sample_size: usize,
    /// Passed to criterion as `--measurement-time`.
    #[serde(default = "default_measurement_time")]
    pub measurement_time: String,
    /// Passed to criterion as `--warm-up-time`.
    #[serde(default = "default_warm_up_time")]
    pub warm_up_time: String,
    /// The largest tolerated significant regression, percent.
    #[serde(default = "default_max_regress")]
    pub max_regress_pct: f64,
    /// The smallest geomean improvement a KEEP will accept: the score must be
    /// below `1 - min_effect_pct/100`, not merely below 1.
    #[serde(default = "default_min_effect")]
    pub min_effect_pct: f64,
    /// Bounds each subprocess phase.
    #[serde(default = "default_timeout")]
    pub timeout: String,
    /// Test files deliberately exempted from freezing. Human-only.
    #[serde(default)]
    pub unfreeze: Vec<String>,
}

fn default_scope() -> Vec<String> {
    vec!["src/**".to_string()]
}
fn default_count() -> usize {
    10
}
fn default_sample_size() -> usize {
    50
}
fn default_measurement_time() -> String {
    "2s".to_string()
}
fn default_warm_up_time() -> String {
    "1s".to_string()
}
fn default_max_regress() -> f64 {
    5.0
}
fn default_min_effect() -> f64 {
    1.0
}
fn default_timeout() -> String {
    "15m".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            benchmarks: Vec::new(),
            bench_targets: Vec::new(),
            scope: default_scope(),
            count: default_count(),
            sample_size: default_sample_size(),
            measurement_time: default_measurement_time(),
            warm_up_time: default_warm_up_time(),
            max_regress_pct: default_max_regress(),
            min_effect_pct: default_min_effect(),
            timeout: default_timeout(),
            unfreeze: Vec::new(),
        }
    }
}

impl Config {
    /// Reads a config file, applying defaults for omitted fields.
    pub fn load(path: &Path) -> Result<Config, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("read config {}: {e}", path.display()))?;
        let cfg: Config =
            serde_yaml::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
        cfg.validate()
            .map_err(|e| format!("invalid {}: {e}", path.display()))?;
        Ok(cfg)
    }

    /// Reports whether the configuration is usable.
    pub fn validate(&self) -> Result<(), String> {
        if self.count < MIN_COUNT {
            return Err(format!(
                "count must be at least {MIN_COUNT}: the significance test cannot report \
                 p < 0.05 with fewer than {MIN_COUNT} measured rounds per side no matter how \
                 large the improvement is, so every experiment would be discarded regardless \
                 of what changed (the default is 10)"
            ));
        }
        if self.sample_size < MIN_SAMPLE_SIZE {
            return Err(format!(
                "sample_size must be at least {MIN_SAMPLE_SIZE}: criterion refuses anything \
                 smaller"
            ));
        }
        if self.max_regress_pct < 0.0 {
            return Err("max_regress_pct must not be negative".to_string());
        }
        if self.min_effect_pct < 0.0 || self.min_effect_pct >= 100.0 {
            return Err("min_effect_pct must be at least 0 and less than 100".to_string());
        }
        if self.scope.is_empty() {
            return Err("scope must list at least one path pattern".to_string());
        }
        if self.scope.iter().any(|s| s.trim().is_empty()) {
            return Err("scope must not contain an empty or whitespace-only entry".to_string());
        }
        parse_duration(&self.measurement_time).map_err(|e| format!("measurement_time: {e}"))?;
        parse_duration(&self.warm_up_time).map_err(|e| format!("warm_up_time: {e}"))?;
        self.timeout_duration()?;
        Ok(())
    }

    /// Parses [`Config::timeout`].
    pub fn timeout_duration(&self) -> Result<Duration, String> {
        parse_duration(&self.timeout).map_err(|e| format!("timeout: {e}"))
    }

    /// Renders `measurement_time` and `warm_up_time` as the decimal seconds
    /// criterion's CLI expects.
    pub fn measurement_secs(&self) -> Result<f64, String> {
        Ok(parse_duration(&self.measurement_time)?.as_secs_f64())
    }

    /// Seconds for criterion's `--warm-up-time`.
    pub fn warm_up_secs(&self) -> Result<f64, String> {
        Ok(parse_duration(&self.warm_up_time)?.as_secs_f64())
    }
}

/// Parses a Go-style duration: an integer or decimal followed by
/// `ms`, `s`, `m` or `h`. A bare number is refused, because "2" is
/// ambiguous between seconds and milliseconds and guessing wrong silently
/// changes how long every round takes.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("duration must not be empty".to_string());
    }
    let (num, unit) = if let Some(rest) = s.strip_suffix("ms") {
        (rest, 0.001)
    } else if let Some(rest) = s.strip_suffix('s') {
        (rest, 1.0)
    } else if let Some(rest) = s.strip_suffix('m') {
        (rest, 60.0)
    } else if let Some(rest) = s.strip_suffix('h') {
        (rest, 3600.0)
    } else {
        return Err(format!("duration {s:?} needs a unit: ms, s, m or h"));
    };
    let value: f64 = num
        .parse()
        .map_err(|_| format!("duration {s:?} does not start with a number"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("duration {s:?} must be a non-negative number"));
    }
    Ok(Duration::from_secs_f64(value * unit))
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn omitted_fields_take_defaults() {
        let f = write("benchmarks:\n  - count_words\n");
        let c = Config::load(f.path()).unwrap();
        assert_eq!(c.benchmarks, vec!["count_words"]);
        assert_eq!(c.count, 10);
        assert_eq!(c.sample_size, 50);
        assert_eq!(c.measurement_time, "2s");
        assert_eq!(c.warm_up_time, "1s");
        assert_eq!(c.max_regress_pct, 5.0);
        assert_eq!(c.min_effect_pct, 1.0);
        assert_eq!(c.timeout, "15m");
        assert_eq!(c.scope, vec!["src/**"]);
    }

    #[test]
    fn bench_targets_round_trip() {
        let f = write("bench_targets:\n  - package: demo\n    target: wordcount\n");
        let c = Config::load(f.path()).unwrap();
        assert_eq!(
            c.bench_targets,
            vec![BenchTarget {
                package: "demo".into(),
                target: "wordcount".into()
            }]
        );
    }

    // The count floor is not a style preference: below 4 rounds per side the
    // exact Mann-Whitney test cannot produce p < 0.05 however large the
    // improvement, so every experiment would discard on a technicality.
    #[test]
    fn count_below_four_is_refused_with_an_explanation() {
        let mut c = Config::default();
        c.count = 3;
        let err = c.validate().unwrap_err();
        assert!(err.contains("at least 4"), "{err}");
        assert!(err.contains("significance"), "must explain why: {err}");
    }

    #[test]
    fn empty_scope_is_refused() {
        let mut c = Config::default();
        c.scope = vec![];
        assert!(c.validate().unwrap_err().contains("at least one"));
    }

    #[test]
    fn whitespace_only_scope_entry_is_refused() {
        let mut c = Config::default();
        c.scope = vec!["   ".into()];
        assert!(c.validate().unwrap_err().contains("empty"));
    }

    #[test]
    fn min_effect_pct_must_be_a_percentage() {
        let mut c = Config::default();
        c.min_effect_pct = 100.0;
        assert!(c.validate().is_err());
        c.min_effect_pct = -1.0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn negative_max_regress_pct_is_refused() {
        let mut c = Config::default();
        c.max_regress_pct = -0.1;
        assert!(c.validate().is_err());
    }

    #[test]
    fn sample_size_below_criterion_floor_is_refused() {
        let mut c = Config::default();
        c.sample_size = 9;
        let err = c.validate().unwrap_err();
        assert!(err.contains("10"), "criterion's own floor is 10: {err}");
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("2").is_err());
        assert!(parse_duration("2x").is_err());
    }

    #[test]
    fn bad_durations_are_refused_by_validate() {
        for bad in ["measurement_time", "warm_up_time", "timeout"] {
            let mut c = Config::default();
            match bad {
                "measurement_time" => c.measurement_time = "nope".into(),
                "warm_up_time" => c.warm_up_time = "nope".into(),
                _ => c.timeout = "nope".into(),
            }
            assert!(c.validate().is_err(), "{bad} must be validated");
        }
    }

    #[test]
    fn unknown_keys_are_refused_rather_than_ignored() {
        // A typo'd key that silently defaults is how a run ends up measuring
        // something other than what the config says.
        let f = write("benchmarkz:\n  - oops\n");
        assert!(Config::load(f.path()).is_err());
    }
}
