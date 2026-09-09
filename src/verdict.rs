//! Turns gate outcomes and measurements into a single decision.

use crate::bench::Delta;
use crate::stats;
use serde::Serialize;

/// The terminal outcome of one experiment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Status {
    #[serde(rename = "KEEP")]
    Keep,
    #[serde(rename = "DISCARD")]
    Discard,
    #[serde(rename = "FAIL")]
    Fail,
    #[serde(rename = "CRASH")]
    Crash,
    /// Not a verdict: an experiment abandoned by `stop --force`. Reported as
    /// exit code 2 so a loop that does not recognise it treats it like FAIL.
    #[serde(rename = "ABORTED")]
    Aborted,
}

/// A machine-readable explanation for a [`Status`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Improved,
    NoSignificantImprovement,
    ImprovementBelowMinEffect,
    GuardRegression,
    ScopeViolation,
    ConfigChanged,
    InlineTestModified,
    NewTestFile,
    MissingTestFile,
    SymlinkSwap,
    FrozenStoreTampered,
    BaselineTampered,
    BuildFailed,
    TestsFailed,
    Timeout,
    StopForced,
}

/// Everything [`decide`] needs once all correctness gates have passed.
#[derive(Clone, Debug)]
pub struct Input {
    pub deltas: Vec<Delta>,
    pub score: f64,
    pub max_regress_pct: f64,
    /// The smallest geomean improvement that qualifies for KEEP: `score` must
    /// be below `1 - min_effect_pct/100`.
    pub min_effect_pct: f64,
}

/// The harness's answer for one experiment.
#[derive(Clone, Debug, Serialize)]
pub struct VerdictResult {
    pub status: Status,
    pub reason: Reason,
    pub score: f64,
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub regressions: Vec<Delta>,
    /// Says when the measurement behind this result is too weak to support it.
    /// Never changes the decision — explains what it can and cannot mean.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl VerdictResult {
    /// The process exit code documented in `program.md`.
    pub fn exit_code(&self) -> i32 {
        match self.status {
            Status::Keep => 0,
            Status::Discard => 1,
            Status::Fail | Status::Aborted => 2,
            Status::Crash => 3,
        }
    }
}

/// Builds a result for a failed correctness stage, before measurement.
pub fn gate(status: Status, reason: Reason, message: impl Into<String>) -> VerdictResult {
    VerdictResult {
        status,
        reason,
        score: 0.0,
        message: message.into(),
        regressions: Vec::new(),
        warnings: Vec::new(),
    }
}

/// Applies the scoring rules.
///
/// 1. Any regression significant at the raw, **uncorrected** alpha and larger
///    than `max_regress_pct` rejects the change however good the overall
///    score. This deliberately skips the Bonferroni correction from rule 2:
///    the correction only ever makes it harder to call a result significant,
///    so applying it here would make the guard less sensitive to harm —
///    backwards from what a guard is for. Be conservative about accepting a
///    win, liberal about catching damage.
/// 2. Otherwise keep only when both:
///    a. the score is a real speedup by at least `min_effect_pct` — below
///    `1 - min_effect_pct/100`, not merely below 1, because a technically
///    significant but trivially small win is not worth a commit in an
///    unattended loop; and
///    b. at least one benchmark improved at the Bonferroni-corrected
///    threshold `alpha/k`, where `k` is the number of benchmarks compared.
///    Testing `k` benchmarks against the same uncorrected alpha inflates
///    the family-wise false-positive rate — with 4 benchmarks, roughly an
///    18% chance one looks significant when nothing changed.
///
/// A change clearing 2b but missing 2a discards as
/// [`Reason::ImprovementBelowMinEffect`]: it measurably worked, it was just
/// too small to bank.
pub fn decide(input: Input) -> VerdictResult {
    let k = input.deltas.len().max(1);
    let warnings = measurement_warnings(&input.deltas, k);

    let regressions: Vec<Delta> = input
        .deltas
        .iter()
        .filter(|d| d.significant && d.pct_change > input.max_regress_pct)
        .cloned()
        .collect();
    if !regressions.is_empty() {
        let detail = regressions
            .iter()
            .map(|d| format!("{} {:+.1}%", d.name, d.pct_change))
            .collect::<Vec<_>>()
            .join(", ");
        return VerdictResult {
            status: Status::Discard,
            reason: Reason::GuardRegression,
            score: input.score,
            message: format!(
                "regression guard tripped (limit {:+.1}%): {detail}",
                input.max_regress_pct
            ),
            regressions,
            warnings,
        };
    }

    let improved = input
        .deltas
        .iter()
        .any(|d| d.pct_change < 0.0 && d.p < d.alpha / k as f64);
    let threshold = 1.0 - input.min_effect_pct / 100.0;

    if input.score < threshold && improved {
        return VerdictResult {
            status: Status::Keep,
            reason: Reason::Improved,
            score: input.score,
            message: format!(
                "score {:.4} ({:+.2}%)",
                input.score,
                (input.score - 1.0) * 100.0
            ),
            regressions: Vec::new(),
            warnings,
        };
    }

    if improved && input.score < 1.0 {
        return VerdictResult {
            status: Status::Discard,
            reason: Reason::ImprovementBelowMinEffect,
            score: input.score,
            message: format!(
                "score {:.4} ({:+.2}%), a real improvement but below the {:.1}% minimum \
                 effect size",
                input.score,
                (input.score - 1.0) * 100.0,
                input.min_effect_pct
            ),
            regressions: Vec::new(),
            warnings,
        };
    }

    VerdictResult {
        status: Status::Discard,
        reason: Reason::NoSignificantImprovement,
        score: input.score,
        message: format!(
            "score {:.4} ({:+.2}%), no significant improvement",
            input.score,
            (input.score - 1.0) * 100.0
        ),
        regressions: Vec::new(),
        warnings,
    }
}

/// Everything that qualifies how far a result can be trusted: the deltas' own
/// warnings, then whether a KEEP was statistically reachable at all.
fn measurement_warnings(deltas: &[Delta], k: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for d in deltas {
        for w in &d.warnings {
            if !out.contains(w) {
                out.push(w.clone());
            }
        }
    }
    if let Some(w) = unreachable_alpha_warning(deltas, k) {
        out.push(w);
    }
    out
}

/// Reports when rule 2b cannot be satisfied by any result whatsoever, so the
/// run is incapable of a KEEP before it starts.
///
/// The U test has a floor on the p-value it can produce for a given sample
/// size. If `alpha/k` falls below that floor for *every* benchmark, none can
/// clear it and every experiment discards no matter what the agent does.
/// `Config::validate` enforces a count floor for one benchmark; this is the
/// same footgun at `k` of them, which the validator cannot see because it does
/// not know how many benchmarks a run will compare.
///
/// A KEEP needs only one benchmark to clear the bar, so this warns only when
/// none of them can.
fn unreachable_alpha_warning(deltas: &[Delta], k: usize) -> Option<String> {
    if deltas.is_empty() {
        return None;
    }
    let mut worst_n = 0usize;
    let mut alpha = 0.0;
    for d in deltas {
        let corrected = d.alpha / k as f64;
        if stats::min_achievable_p(d.n_base, d.n_cand) < corrected {
            return None; // this one can clear it; that is enough for a KEEP
        }
        let n = d.n_base.min(d.n_cand);
        if n > worst_n {
            worst_n = n;
            alpha = d.alpha;
        }
    }
    let corrected = alpha / k as f64;
    let mut msg = format!(
        "no KEEP was reachable: comparing {k} benchmark(s) corrects the significance threshold \
         to {corrected:.5}, but with {worst_n} rounds per side the test cannot produce a \
         p-value below {:.5} however large the improvement is",
        stats::min_achievable_p(worst_n, worst_n)
    );
    match stats::count_for_alpha(corrected) {
        Some(need) => msg.push_str(&format!(" — raise count to at least {need}")),
        None => msg.push_str(" — raise count, or measure fewer benchmarks"),
    }
    Some(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::Delta;

    fn delta(name: &str, pct: f64, p: f64) -> Delta {
        let ratio = 1.0 + pct / 100.0;
        Delta {
            name: name.to_string(),
            base_center: 1000.0,
            cand_center: 1000.0 * ratio,
            ratio,
            pct_change: pct,
            p,
            alpha: 0.05,
            significant: p < 0.05,
            n_base: 10,
            n_cand: 10,
            warnings: Vec::new(),
        }
    }

    fn input(deltas: Vec<Delta>, score: f64) -> Input {
        Input {
            deltas,
            score,
            max_regress_pct: 5.0,
            min_effect_pct: 1.0,
        }
    }

    #[test]
    fn a_real_significant_improvement_is_kept() {
        let r = decide(input(vec![delta("a", -20.0, 0.001)], 0.80));
        assert_eq!(r.status, Status::Keep);
        assert_eq!(r.reason, Reason::Improved);
        assert_eq!(r.exit_code(), 0);
    }

    #[test]
    fn no_movement_discards_as_no_significant_improvement() {
        let r = decide(input(vec![delta("a", -0.1, 0.9)], 0.999));
        assert_eq!(r.status, Status::Discard);
        assert_eq!(r.reason, Reason::NoSignificantImprovement);
        assert_eq!(r.exit_code(), 1);
    }

    // A real but tiny win must be distinguishable from an inert one: the two
    // call for different next moves, and reporting the first as the second
    // would tell an agent its change did nothing when it measurably did.
    #[test]
    fn a_real_win_below_the_effect_floor_says_so() {
        let r = decide(input(vec![delta("a", -0.5, 0.0001)], 0.995));
        assert_eq!(r.status, Status::Discard);
        assert_eq!(r.reason, Reason::ImprovementBelowMinEffect);
        assert!(r.message.contains("real improvement"), "{}", r.message);
    }

    #[test]
    fn a_significant_regression_past_the_limit_trips_the_guard() {
        let r = decide(input(
            vec![delta("fast", -30.0, 0.001), delta("slow", 20.0, 0.001)],
            0.85,
        ));
        assert_eq!(r.status, Status::Discard);
        assert_eq!(r.reason, Reason::GuardRegression);
        assert_eq!(r.regressions.len(), 1);
        assert_eq!(r.regressions[0].name, "slow");
        assert!(r.message.contains("slow"), "{}", r.message);
    }

    #[test]
    fn an_insignificant_regression_does_not_trip_the_guard() {
        let r = decide(input(
            vec![delta("a", -30.0, 0.001), delta("b", 20.0, 0.9)],
            0.85,
        ));
        assert_eq!(r.status, Status::Keep);
    }

    #[test]
    fn a_regression_within_the_limit_does_not_trip_the_guard() {
        let r = decide(input(
            vec![delta("a", -30.0, 0.001), delta("b", 3.0, 0.001)],
            0.85,
        ));
        assert_eq!(r.status, Status::Keep);
    }

    // The guard uses the RAW alpha while the KEEP rule uses alpha/k. Applying
    // the correction here too would make real regressions easier to miss —
    // backwards from what a guard is for.
    #[test]
    fn the_guard_is_not_bonferroni_corrected() {
        // p = 0.04 clears the raw alpha but not 0.05/4 = 0.0125.
        let deltas = vec![
            delta("a", -30.0, 0.0001),
            delta("b", 20.0, 0.04),
            delta("c", 0.0, 0.9),
            delta("d", 0.0, 0.9),
        ];
        let r = decide(input(deltas, 0.85));
        assert_eq!(
            r.status,
            Status::Discard,
            "the raw-alpha regression must still bite"
        );
        assert_eq!(r.reason, Reason::GuardRegression);
    }

    // With k benchmarks, a KEEP needs one of them below alpha/k.
    #[test]
    fn keep_requires_the_bonferroni_corrected_threshold() {
        let deltas = vec![
            delta("a", -20.0, 0.02), // clears 0.05, not 0.05/4 = 0.0125
            delta("b", 0.0, 0.9),
            delta("c", 0.0, 0.9),
            delta("d", 0.0, 0.9),
        ];
        let r = decide(input(deltas, 0.90));
        assert_eq!(r.status, Status::Discard);
        assert_eq!(r.reason, Reason::NoSignificantImprovement);
    }

    #[test]
    fn one_benchmark_clearing_the_corrected_bar_is_enough() {
        let deltas = vec![
            delta("a", -20.0, 0.001), // clears 0.05/4
            delta("b", 0.0, 0.9),
            delta("c", 0.0, 0.9),
            delta("d", 0.0, 0.9),
        ];
        let r = decide(input(deltas, 0.90));
        assert_eq!(r.status, Status::Keep);
    }

    #[test]
    fn a_score_at_the_effect_threshold_is_not_kept() {
        // min_effect_pct 1.0 means the score must be strictly below 0.99.
        let r = decide(input(vec![delta("a", -1.0, 0.0001)], 0.99));
        assert_eq!(r.status, Status::Discard);
        assert_eq!(r.reason, Reason::ImprovementBelowMinEffect);
    }

    #[test]
    fn warnings_from_deltas_are_carried_through_and_deduplicated() {
        let mut a = delta("a", -20.0, 0.001);
        let mut b = delta("b", -20.0, 0.001);
        a.warnings = vec!["same warning".into()];
        b.warnings = vec!["same warning".into(), "other".into()];
        let r = decide(input(vec![a, b], 0.80));
        assert_eq!(r.warnings.len(), 2, "{:?}", r.warnings);
    }

    // With enough benchmarks, alpha/k falls below the p-value floor for the
    // round count and no KEEP is reachable at all — before the agent tries
    // anything. Say so rather than discarding silently forever.
    #[test]
    fn an_unreachable_corrected_alpha_is_warned_about() {
        let deltas: Vec<Delta> = (0..7)
            .map(|i| {
                let mut d = delta(&format!("b{i}"), -5.0, 0.02);
                d.n_base = 5;
                d.n_cand = 5;
                d
            })
            .collect();
        let r = decide(input(deltas, 0.95));
        let joined = r.warnings.join(" ");
        assert!(joined.contains("no KEEP was reachable"), "{joined}");
        assert!(joined.contains("raise count to at least 6"), "{joined}");
    }

    #[test]
    fn a_reachable_alpha_produces_no_such_warning() {
        let deltas: Vec<Delta> = (0..2)
            .map(|i| delta(&format!("b{i}"), -20.0, 0.001))
            .collect();
        let r = decide(input(deltas, 0.80));
        assert!(
            !r.warnings.join(" ").contains("no KEEP was reachable"),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn gates_carry_their_reason_and_exit_code() {
        let r = gate(Status::Fail, Reason::ScopeViolation, "out of scope");
        assert_eq!(r.exit_code(), 2);
        assert_eq!(r.status, Status::Fail);
        let r = gate(Status::Crash, Reason::BuildFailed, "boom");
        assert_eq!(r.exit_code(), 3);
        // ABORTED is not a verdict; program.md treats it like FAIL.
        let r = gate(Status::Aborted, Reason::StopForced, "abandoned");
        assert_eq!(r.exit_code(), 2);
    }

    #[test]
    fn statuses_and_reasons_serialize_as_the_documented_strings() {
        let r = gate(Status::Fail, Reason::InlineTestModified, "m");
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"status\":\"FAIL\""), "{json}");
        assert!(
            json.contains("\"reason\":\"inline_test_modified\""),
            "{json}"
        );
    }

    #[test]
    fn an_empty_delta_set_does_not_divide_by_zero() {
        let r = decide(input(vec![], 1.0));
        assert_eq!(r.status, Status::Discard);
    }
}
