//! Reads criterion's JSON output into measurement sets and compares them.
//!
//! Criterion collects 100 samples per invocation and computes its own
//! statistics; the harness takes a single number out of that — the median
//! point estimate — as one observation, and runs its own interleaved
//! comparison across rounds. That is exactly the shape `go test -bench` gives
//! the Go original: one figure per round, `count` rounds, Mann-Whitney across
//! them.
//!
//! Criterion's own change detection (`base/` against `new/`) is deliberately
//! unused: it compares a run against whatever ran previously in the same
//! directory, which is neither interleaved nor the comparison this tool makes.

use crate::stats;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// One benchmark's observations, in observation order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Set {
    series: BTreeMap<String, Vec<f64>>,
}

impl Set {
    pub fn new() -> Self {
        Set {
            series: BTreeMap::new(),
        }
    }

    /// Appends one observation, in nanoseconds.
    pub fn record(&mut self, name: &str, ns: f64) {
        self.series.entry(name.to_string()).or_default().push(ns);
    }

    /// Every benchmark name, sorted (a `BTreeMap` is already ordered).
    pub fn names(&self) -> Vec<String> {
        self.series.keys().cloned().collect()
    }

    /// A copy of one benchmark's observations, safe for the caller to mutate.
    pub fn values(&self, name: &str) -> Option<Vec<f64>> {
        self.series.get(name).cloned()
    }

    /// Whether any observation exists for `name`, without the copy.
    pub fn has(&self, name: &str) -> bool {
        self.series.contains_key(name)
    }

    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }

    /// Appends every observation in `other`, preserving order.
    pub fn add(&mut self, other: &Set) {
        for (name, values) in &other.series {
            self.series.entry(name.clone()).or_default().extend(values);
        }
    }
}

#[derive(Deserialize)]
struct BenchmarkMeta {
    full_id: String,
}

#[derive(Deserialize)]
struct PointEstimate {
    point_estimate: f64,
}

#[derive(Deserialize)]
struct Estimates {
    median: PointEstimate,
}

/// Reads every benchmark result under one `CRITERION_HOME`.
///
/// Walks for `new/estimates.json` files and takes `median.point_estimate`,
/// naming each by the `full_id` in the sibling `benchmark.json` — the name the
/// user configured and the one the criterion filter regex must match. A
/// missing directory yields an empty set rather than an error: what a missing
/// benchmark *means* is the caller's decision, and [`crate::measure`] makes it.
pub fn read_round(criterion_home: &Path) -> Result<Set, String> {
    let mut set = Set::new();
    if !criterion_home.exists() {
        return Ok(set);
    }
    visit(criterion_home, &mut set)?;
    Ok(set)
}

fn visit(dir: &Path, set: &mut Set) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("read criterion output {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read criterion output: {e}"))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if entry.file_name() == "new" {
            read_one(&path, set)?;
            continue;
        }
        // "report" holds criterion's HTML and never any estimates; "base" is
        // its own previous run, which this tool deliberately ignores.
        if entry.file_name() == "report" || entry.file_name() == "base" {
            continue;
        }
        visit(&path, set)?;
    }
    Ok(())
}

fn read_one(new_dir: &Path, set: &mut Set) -> Result<(), String> {
    let est_path = new_dir.join("estimates.json");
    let meta_path = new_dir.join("benchmark.json");
    if !est_path.exists() || !meta_path.exists() {
        return Ok(());
    }
    let meta: BenchmarkMeta = serde_json::from_str(
        &std::fs::read_to_string(&meta_path)
            .map_err(|e| format!("read {}: {e}", meta_path.display()))?,
    )
    .map_err(|e| format!("parse {}: {e}", meta_path.display()))?;
    let est: Estimates = serde_json::from_str(
        &std::fs::read_to_string(&est_path)
            .map_err(|e| format!("read {}: {e}", est_path.display()))?,
    )
    .map_err(|e| format!("parse {}: {e}", est_path.display()))?;
    set.record(&meta.full_id, est.median.point_estimate);
    Ok(())
}

/// The comparison of one benchmark between baseline and candidate.
#[derive(Clone, Debug, Serialize)]
pub struct Delta {
    pub name: String,
    /// Median nanoseconds under the distribution-free summary.
    pub base_center: f64,
    pub cand_center: f64,
    /// `cand_center / base_center`. Below 1 means the candidate is faster.
    pub ratio: f64,
    /// `(ratio - 1) * 100`.
    pub pct_change: f64,
    /// The Mann-Whitney p-value and the raw, uncorrected rejection threshold.
    pub p: f64,
    pub alpha: f64,
    /// `p < alpha`, always at the RAW alpha. The Bonferroni correction in
    /// [`crate::verdict`] is a KEEP-decision threshold layered on top, never a
    /// redefinition of what "significant" means in a report a human reads.
    pub significant: bool,
    pub n_base: usize,
    pub n_cand: usize,
    /// Warnings from the two summaries and the comparison, deduplicated.
    pub warnings: Vec<String>,
}

/// Compares one benchmark across two measurement sets.
fn compare_one(base: &Set, cand: &Set, name: &str) -> Result<Delta, String> {
    let bv = base
        .values(name)
        .ok_or_else(|| format!("baseline has no {name}"))?;
    let cv = cand
        .values(name)
        .ok_or_else(|| format!("candidate has no {name}"))?;
    if bv.len() < 2 || cv.len() < 2 {
        return Err(format!(
            "{name}: need at least 2 observations per side, got {}/{}",
            bv.len(),
            cv.len()
        ));
    }

    let b_sum = stats::summary(&bv, stats::CONFIDENCE);
    let c_sum = stats::summary(&cv, stats::CONFIDENCE);
    let cmp = stats::compare(&bv, &cv);

    if b_sum.center == 0.0 {
        return Err(format!(
            "{name}: baseline median is zero, cannot form a ratio"
        ));
    }
    let ratio = c_sum.center / b_sum.center;

    let mut warnings = Vec::new();
    for w in b_sum
        .warnings
        .iter()
        .chain(&c_sum.warnings)
        .chain(&cmp.warnings)
    {
        if !warnings.contains(w) {
            warnings.push(w.clone());
        }
    }

    Ok(Delta {
        name: name.to_string(),
        base_center: b_sum.center,
        cand_center: c_sum.center,
        ratio,
        pct_change: (ratio - 1.0) * 100.0,
        p: cmp.p,
        alpha: cmp.alpha,
        significant: cmp.p < cmp.alpha,
        n_base: cmp.n1,
        n_cand: cmp.n2,
        warnings,
    })
}

/// Compares every benchmark measured at baseline, sorted by name.
///
/// A benchmark present at baseline but absent from the candidate is an error,
/// not a skip: it cannot be checked for regressions, so passing over it would
/// hide real damage behind a clean-looking score.
pub fn compare_all(base: &Set, cand: &Set) -> Result<Vec<Delta>, String> {
    let mut out = Vec::new();
    let mut missing = Vec::new();
    for name in base.names() {
        if !cand.has(&name) {
            missing.push(name);
            continue;
        }
        out.push(compare_one(base, cand, &name)?);
    }
    if !missing.is_empty() {
        return Err(format!(
            "benchmark(s) measured at baseline but missing from the candidate: {missing:?} — \
             a benchmark that disappears cannot be checked for regressions"
        ));
    }
    if out.is_empty() {
        return Err("no benchmark appears in both baseline and candidate".to_string());
    }
    Ok(out)
}

/// The geometric mean of the deltas' ratios — the single score the agent
/// optimizes. Below 1 is an overall speedup.
pub fn geomean(deltas: &[Delta]) -> Result<f64, String> {
    if deltas.is_empty() {
        return Err("geomean of an empty delta set".to_string());
    }
    let mut sum = 0.0;
    for d in deltas {
        if d.ratio <= 0.0 {
            return Err(format!("{}: non-positive ratio {}", d.name, d.ratio));
        }
        sum += d.ratio.ln();
    }
    Ok((sum / deltas.len() as f64).exp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/criterion")
    }

    #[test]
    fn reads_the_median_point_estimate_in_nanoseconds() {
        let set = read_round(&fixtures()).unwrap();
        let v = set.values("count_words").expect("count_words present");
        assert_eq!(v.len(), 1, "one round yields exactly one observation");
        assert!((v[0] - 44165.9866603892).abs() < 1e-6, "got {}", v[0]);
    }

    // A grouped benchmark's full_id contains a slash and its directory is
    // nested; the name the harness uses must be the full_id, because that is
    // what the criterion filter regex has to match.
    #[test]
    fn grouped_benchmarks_are_named_by_full_id() {
        let set = read_round(&fixtures()).unwrap();
        assert!(set.has("parse/small"), "names: {:?}", set.names());
        assert_eq!(set.values("parse/small").unwrap(), vec![100.0]);
    }

    #[test]
    fn names_are_sorted() {
        let set = read_round(&fixtures()).unwrap();
        assert_eq!(set.names(), vec!["count_words", "parse/small"]);
    }

    #[test]
    fn a_missing_criterion_home_is_an_empty_set_not_an_error() {
        // The caller decides what a missing benchmark means; read_round only
        // reports what it found.
        let set = read_round(&fixtures().join("does-not-exist")).unwrap();
        assert!(set.is_empty());
    }

    fn set_of(name: &str, values: &[f64]) -> Set {
        let mut s = Set::new();
        for &v in values {
            s.record(name, v);
        }
        s
    }

    #[test]
    fn add_accumulates_observations_in_order() {
        let mut a = set_of("b", &[1.0, 2.0]);
        a.add(&set_of("b", &[3.0]));
        assert_eq!(a.values("b").unwrap(), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn delta_ratio_and_pct_change_are_consistent() {
        let base = set_of("b", &[100.0, 101.0, 102.0, 103.0, 104.0, 105.0]);
        let cand = set_of("b", &[50.0, 51.0, 52.0, 53.0, 54.0, 55.0]);
        let deltas = compare_all(&base, &cand).unwrap();
        assert_eq!(deltas.len(), 1);
        let d = &deltas[0];
        assert!((d.ratio - 0.5).abs() < 0.02, "ratio {}", d.ratio);
        assert!((d.pct_change - (d.ratio - 1.0) * 100.0).abs() < 1e-9);
        assert!(d.significant, "clean halving must be significant");
        assert_eq!(d.n_base, 6);
        assert_eq!(d.n_cand, 6);
    }

    // A benchmark that vanishes from the candidate cannot be checked for
    // regressions, which would hide a real problem behind a clean-looking score.
    #[test]
    fn a_benchmark_missing_from_the_candidate_is_an_error() {
        let mut base = set_of("a", &[1.0, 2.0, 3.0, 4.0]);
        base.add(&set_of("b", &[1.0, 2.0, 3.0, 4.0]));
        let cand = set_of("a", &[1.0, 2.0, 3.0, 4.0]);
        let err = compare_all(&base, &cand).unwrap_err();
        assert!(err.contains("b"), "{err}");
        assert!(err.contains("missing"), "{err}");
    }

    #[test]
    fn comparing_disjoint_sets_is_an_error() {
        let base = set_of("a", &[1.0, 2.0, 3.0, 4.0]);
        let cand = set_of("z", &[1.0, 2.0, 3.0, 4.0]);
        assert!(compare_all(&base, &cand).is_err());
    }

    #[test]
    fn fewer_than_two_observations_per_side_is_an_error() {
        let base = set_of("a", &[1.0]);
        let cand = set_of("a", &[1.0]);
        let err = compare_all(&base, &cand).unwrap_err();
        assert!(err.contains("at least 2"), "{err}");
    }

    #[test]
    fn a_zero_baseline_median_cannot_form_a_ratio() {
        let base = set_of("a", &[0.0, 0.0, 0.0, 0.0]);
        let cand = set_of("a", &[1.0, 2.0, 3.0, 4.0]);
        assert!(compare_all(&base, &cand).unwrap_err().contains("zero"));
    }

    fn delta_with_ratio(name: &str, ratio: f64) -> Delta {
        Delta {
            name: name.to_string(),
            base_center: 1.0,
            cand_center: ratio,
            ratio,
            pct_change: (ratio - 1.0) * 100.0,
            p: 0.01,
            alpha: 0.05,
            significant: true,
            n_base: 10,
            n_cand: 10,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn geomean_of_one_delta_is_its_ratio() {
        assert!((geomean(&[delta_with_ratio("a", 0.5)]).unwrap() - 0.5).abs() < 1e-12);
    }

    #[test]
    fn geomean_multiplies_not_averages() {
        // sqrt(0.25 * 4) == 1.0: a 4x win and a 4x loss cancel exactly.
        let d = [delta_with_ratio("a", 0.25), delta_with_ratio("b", 4.0)];
        assert!((geomean(&d).unwrap() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn geomean_of_nothing_is_an_error() {
        assert!(geomean(&[]).is_err());
    }

    #[test]
    fn geomean_refuses_a_non_positive_ratio() {
        assert!(geomean(&[delta_with_ratio("a", 0.0)]).is_err());
    }
}
