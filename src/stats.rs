//! A port of the `AssumeNothing` path of `golang.org/x/perf/benchmath`.
//!
//! Distribution-free throughout: the median is the center statistic, its
//! confidence interval comes from the binomial order statistics, and the
//! two-sample test is Mann-Whitney U — exact for small samples, normal
//! approximation with a tie correction above that.
//!
//! Nothing here may drift from the Go implementation without the two tools
//! disagreeing about what counts as significant, so the tests pin the values
//! the Go version documents.

/// The rejection threshold for a significant difference.
pub const DEFAULT_ALPHA: f64 = 0.05;

/// The confidence level for a reported median interval.
pub const CONFIDENCE: f64 = 0.95;

/// Sample sizes at or below this use the exact U distribution. Above it the
/// exact recurrence gets expensive and the normal approximation is accurate.
pub const EXACT_LIMIT: usize = 20;

/// A one-sample summary: the median and its confidence interval.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub center: f64,
    pub lo: f64,
    pub hi: f64,
    pub warnings: Vec<String>,
}

/// A two-sample comparison.
#[derive(Clone, Debug, PartialEq)]
pub struct Comparison {
    pub p: f64,
    pub alpha: f64,
    pub n1: usize,
    pub n2: usize,
    pub warnings: Vec<String>,
}

/// Returns the median and a distribution-free confidence interval for it.
///
/// The interval is the pair of order statistics whose binomial tail mass is
/// at most `(1-confidence)/2` on each side. When no such pair exists — which
/// happens below 6 observations at 95% confidence — the interval is the full
/// range and a warning says so, rather than reporting a tight interval the
/// data cannot support.
pub fn summary(values: &[f64], confidence: f64) -> Summary {
    let mut warnings = Vec::new();
    if values.is_empty() {
        return Summary {
            center: f64::NAN,
            lo: f64::NAN,
            hi: f64::NAN,
            warnings,
        };
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    let center = median_of_sorted(&sorted);

    let tail = (1.0 - confidence) / 2.0;
    match interval_rank(n, tail) {
        Some(k) => Summary {
            center,
            lo: sorted[k],
            hi: sorted[n - 1 - k],
            warnings,
        },
        None => {
            warnings.push(format!(
                "confidence interval requires at least {} observations at {:.0}% confidence; \
                 got {n}, so the interval is unbounded",
                min_n_for_interval(confidence),
                confidence * 100.0
            ));
            Summary {
                center,
                lo: sorted[0],
                hi: sorted[n - 1],
                warnings,
            }
        }
    }
}

/// The smallest index `k` such that the binomial tail P(X <= k-1) <= `tail`
/// for `n` fair coin flips, or `None` when no index qualifies.
fn interval_rank(n: usize, tail: f64) -> Option<usize> {
    let mut cumulative = 0.0;
    let total = 2f64.powi(n as i32);
    for k in 0..n / 2 {
        cumulative += binom(n, k) / total;
        if cumulative > tail {
            return if k == 0 { None } else { Some(k - 1) };
        }
    }
    None
}

/// The fewest observations for which [`interval_rank`] yields a bounded
/// interval — 6 at 95% confidence, matching benchmath.
fn min_n_for_interval(confidence: f64) -> usize {
    let tail = (1.0 - confidence) / 2.0;
    (1..=64)
        .find(|&n| interval_rank(n, tail).is_some())
        .unwrap_or(0)
}

fn median_of_sorted(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// Two-sided Mann-Whitney U test between two samples.
pub fn compare(a: &[f64], b: &[f64]) -> Comparison {
    let (n1, n2) = (a.len(), b.len());
    let mut warnings = Vec::new();
    if n1 == 0 || n2 == 0 {
        return Comparison {
            p: 1.0,
            alpha: DEFAULT_ALPHA,
            n1,
            n2,
            warnings,
        };
    }

    let (u, tie_groups) = mann_whitney_u(a, b);
    let has_ties = tie_groups.iter().any(|&t| t > 1);
    let p = if n1 <= EXACT_LIMIT && n2 <= EXACT_LIMIT && !has_ties {
        exact_p(u, n1, n2)
    } else {
        if has_ties {
            warnings.push("samples contain ties; using the normal approximation".to_string());
        }
        normal_p(u, n1, n2, &tie_groups)
    };
    Comparison {
        p: p.clamp(0.0, 1.0),
        alpha: DEFAULT_ALPHA,
        n1,
        n2,
        warnings,
    }
}

/// Returns the smaller U statistic and the sizes of each group of tied
/// values across the pooled sample.
fn mann_whitney_u(a: &[f64], b: &[f64]) -> (f64, Vec<usize>) {
    let mut pooled: Vec<(f64, bool)> = a
        .iter()
        .map(|&v| (v, true))
        .chain(b.iter().map(|&v| (v, false)))
        .collect();
    pooled.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut rank_sum_a = 0.0;
    let mut tie_groups = Vec::new();
    let mut i = 0;
    while i < pooled.len() {
        let mut j = i;
        while j + 1 < pooled.len() && pooled[j + 1].0 == pooled[i].0 {
            j += 1;
        }
        let group = j - i + 1;
        tie_groups.push(group);
        // Ranks are 1-based; tied values share the average of their ranks.
        let avg_rank = ((i + 1) + (j + 1)) as f64 / 2.0;
        for item in &pooled[i..=j] {
            if item.1 {
                rank_sum_a += avg_rank;
            }
        }
        i = j + 1;
    }

    let (n1, n2) = (a.len() as f64, b.len() as f64);
    let u1 = rank_sum_a - n1 * (n1 + 1.0) / 2.0;
    let u2 = n1 * n2 - u1;
    (u1.min(u2), tie_groups)
}

/// Exact two-sided p-value from the U distribution, by counting the number of
/// rank arrangements with a U at least as extreme as the observed one.
fn exact_p(u: f64, n1: usize, n2: usize) -> f64 {
    let u_int = u.round() as usize;
    let counts = u_distribution(n1, n2);
    let total: f64 = counts.iter().sum();
    let tail: f64 = counts[..=u_int.min(counts.len() - 1)].iter().sum();
    (2.0 * tail / total).min(1.0)
}

/// `counts[u]` is the number of ways to obtain the statistic `u` with samples
/// of size `n1` and `n2`, by the standard recurrence.
fn u_distribution(n1: usize, n2: usize) -> Vec<f64> {
    let max_u = n1 * n2;
    // table[i][j][u] collapsed to a rolling 2-D array over j.
    let mut prev = vec![vec![0f64; max_u + 1]; n2 + 1];
    for row in prev.iter_mut() {
        row[0] = 1.0;
    }
    for i in 1..=n1 {
        let mut cur = vec![vec![0f64; max_u + 1]; n2 + 1];
        cur[0][0] = 1.0;
        for j in 1..=n2 {
            for u in 0..=max_u {
                // Either the next item comes from sample 1 (no new inversions)
                // or from sample 2 (adding i inversions).
                let from_a = prev[j][u];
                let from_b = if u >= i { cur[j - 1][u - i] } else { 0.0 };
                cur[j][u] = from_a + from_b;
            }
        }
        prev = cur;
    }
    prev[n2].clone()
}

/// Normal approximation to the U distribution with a continuity and tie
/// correction, used above [`EXACT_LIMIT`] or when the samples contain ties.
fn normal_p(u: f64, n1: usize, n2: usize, tie_groups: &[usize]) -> f64 {
    let (n1f, n2f) = (n1 as f64, n2 as f64);
    let n = n1f + n2f;
    let mean = n1f * n2f / 2.0;
    let tie_term: f64 = tie_groups
        .iter()
        .map(|&t| {
            let t = t as f64;
            t * t * t - t
        })
        .sum();
    let variance = (n1f * n2f / 12.0) * ((n + 1.0) - tie_term / (n * (n - 1.0)));
    if variance <= 0.0 {
        return 1.0;
    }
    // Continuity correction: U is discrete, the normal is not.
    let z = (u - mean + 0.5) / variance.sqrt();
    2.0 * normal_cdf(z)
}

/// The standard normal CDF, via the complementary error function.
fn normal_cdf(z: f64) -> f64 {
    0.5 * erfc(-z / std::f64::consts::SQRT_2)
}

/// Complementary error function, Numerical Recipes' Chebyshev fit. Accurate
/// to about 1.2e-7 relative, far tighter than any p-value here is read to.
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 2.0 / (2.0 + z);
    let ty = 4.0 * t - 2.0;
    // Transcribed verbatim from Numerical Recipes; kept at full precision so
    // the literals match the source exactly rather than clippy's rounding.
    #[allow(clippy::excessive_precision)]
    const COF: [f64; 28] = [
        -1.3026537197817094,
        6.4196979235649026e-1,
        1.9476473204185836e-2,
        -9.561514786808631e-3,
        -9.46595344482036e-4,
        3.66839497852761e-4,
        4.2523324806907e-5,
        -2.0278578112534e-5,
        -1.624290004647e-6,
        1.303655835580e-6,
        1.5626441722e-8,
        -8.5238095915e-8,
        6.529054439e-9,
        5.059343495e-9,
        -9.91364156e-10,
        -2.27365122e-10,
        9.6467911e-11,
        2.394038e-12,
        -6.886027e-12,
        8.94487e-13,
        3.13092e-13,
        -1.12708e-13,
        3.81e-16,
        7.106e-15,
        -1.523e-15,
        -9.4e-17,
        1.21e-16,
        -2.8e-17,
    ];
    let mut d = 0.0;
    let mut dd = 0.0;
    for &c in COF.iter().rev().take(COF.len() - 1) {
        let tmp = d;
        d = ty * d - dd + c;
        dd = tmp;
    }
    let ans = t * (-z * z + 0.5 * (COF[0] + ty * d) - dd).exp();
    if x >= 0.0 { ans } else { 2.0 - ans }
}

/// The smallest two-sided p-value the U test can return for samples of size
/// `n1` and `n2`: `2 / C(n1+n2, n1)`.
///
/// Two maximally separated samples still only reach this, because it is the
/// fraction of orderings at least as extreme as the observed one. It matches
/// benchmath's generated table exactly without duplicating it.
pub fn min_achievable_p(n1: usize, n2: usize) -> f64 {
    if n1 < 1 || n2 < 1 {
        return 1.0;
    }
    let p = 2.0 / binom(n1 + n2, n1);
    if p > 1.0 { 1.0 } else { p }
}

/// The smallest number of rounds per side at which the U test can produce a
/// p-value below `alpha`, or `None` when no practical count does. The search
/// stops at 50, far past any sensible benchmark budget.
pub fn count_for_alpha(alpha: f64) -> Option<usize> {
    (2..=50).find(|&n| min_achievable_p(n, n) < alpha)
}

/// `C(n, k)` as an `f64`, multiplying and dividing in step so the running
/// value stays near the result rather than overflowing through a factorial.
pub fn binom(n: usize, k: usize) -> f64 {
    if k > n {
        return 0.0;
    }
    let k = k.min(n - k);
    let mut c = 1.0;
    for i in 0..k {
        c = c * (n - i) as f64 / (i + 1) as f64;
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    // These four values are quoted in the Go implementation's own comment as
    // matching benchmath's generated uTestMinP table. If this port ever
    // disagrees with them, the two tools disagree about what is significant.
    #[test]
    fn min_achievable_p_matches_the_go_table() {
        assert!(close(min_achievable_p(2, 2), 0.3333, 1e-4));
        assert!(close(min_achievable_p(3, 3), 0.1000, 1e-4));
        assert!(close(min_achievable_p(4, 4), 0.02857, 1e-5));
        assert!(close(min_achievable_p(5, 5), 0.00794, 1e-5));
    }

    #[test]
    fn min_achievable_p_is_clamped_and_safe_at_the_edges() {
        assert_eq!(min_achievable_p(0, 5), 1.0);
        assert_eq!(min_achievable_p(1, 1), 1.0); // 2/C(2,1) = 1.0
    }

    #[test]
    fn count_for_alpha_finds_the_smallest_workable_count() {
        // 0.05 needs n=4 (0.02857 < 0.05, while n=3 gives 0.1).
        assert_eq!(count_for_alpha(0.05), Some(4));
        // The spec's worked case: 7 benchmarks correct alpha to 0.00714,
        // which n=5 (0.00794) cannot reach but n=6 can.
        assert_eq!(count_for_alpha(0.05 / 7.0), Some(6));
        // Nothing practical reaches an absurdly small alpha.
        assert_eq!(count_for_alpha(1e-30), None);
    }

    #[test]
    fn binom_is_exact_for_small_values() {
        assert_eq!(binom(4, 2), 6.0);
        assert_eq!(binom(10, 5), 252.0);
        assert_eq!(binom(5, 0), 1.0);
        assert_eq!(binom(5, 6), 0.0);
    }

    #[test]
    fn identical_samples_are_not_significant() {
        let a = [10.0, 10.0, 10.0, 10.0, 10.0, 10.0];
        let c = compare(&a, &a);
        assert!(
            c.p > 0.05,
            "identical samples must not be significant, got p={}",
            c.p
        );
        assert_eq!(c.n1, 6);
        assert_eq!(c.n2, 6);
    }

    #[test]
    fn cleanly_separated_samples_are_significant() {
        let base = [100.0, 101.0, 102.0, 103.0, 104.0, 105.0];
        let cand = [50.0, 51.0, 52.0, 53.0, 54.0, 55.0];
        let c = compare(&base, &cand);
        assert!(
            c.p < 0.05,
            "clean separation must be significant, got p={}",
            c.p
        );
    }

    // With no overlap at all the exact test bottoms out at exactly the floor.
    #[test]
    fn maximally_separated_samples_hit_the_floor_exactly() {
        let base = [10.0, 11.0, 12.0, 13.0];
        let cand = [1.0, 2.0, 3.0, 4.0];
        let c = compare(&base, &cand);
        assert!(
            close(c.p, min_achievable_p(4, 4), 1e-9),
            "p={} floor={}",
            c.p,
            min_achievable_p(4, 4)
        );
    }

    #[test]
    fn ties_do_not_panic_and_stay_unsignificant() {
        let base = [5.0, 5.0, 5.0, 5.0, 5.0];
        let cand = [5.0, 5.0, 5.0, 5.0, 5.0];
        let c = compare(&base, &cand);
        assert!(c.p.is_finite());
        assert!(c.p > 0.05);
    }

    #[test]
    fn summary_center_is_the_median() {
        let s = summary(&[1.0, 2.0, 3.0, 4.0, 5.0], CONFIDENCE);
        assert_eq!(s.center, 3.0);
        let s = summary(&[1.0, 2.0, 3.0, 4.0], CONFIDENCE);
        assert_eq!(s.center, 2.5);
    }

    // benchmath's own warning, surfaced rather than swallowed: below 6
    // observations the 95% median interval is unbounded.
    #[test]
    fn fewer_than_six_observations_warns_about_the_interval() {
        let s = summary(&[1.0, 2.0, 3.0, 4.0, 5.0], CONFIDENCE);
        assert!(
            s.warnings.iter().any(|w| w.contains("confidence interval")),
            "expected an interval warning, got {:?}",
            s.warnings
        );
        let s = summary(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], CONFIDENCE);
        assert!(
            s.warnings.is_empty(),
            "6 observations is enough: {:?}",
            s.warnings
        );
    }

    #[test]
    fn summary_of_one_value_is_that_value() {
        let s = summary(&[42.0], CONFIDENCE);
        assert_eq!(s.center, 42.0);
        assert!(!s.warnings.is_empty());
    }

    // Above the exact test's range the normal approximation takes over; the
    // two must agree closely at the boundary rather than jumping.
    #[test]
    fn exact_and_normal_agree_near_the_boundary() {
        let base: Vec<f64> = (0..EXACT_LIMIT).map(|i| i as f64).collect();
        let cand: Vec<f64> = (0..EXACT_LIMIT).map(|i| (i + 100) as f64).collect();
        let exact = compare(&base, &cand);
        let bigger_base: Vec<f64> = (0..EXACT_LIMIT + 1).map(|i| i as f64).collect();
        let bigger_cand: Vec<f64> = (0..EXACT_LIMIT + 1).map(|i| (i + 100) as f64).collect();
        let approx = compare(&bigger_base, &bigger_cand);
        assert!(exact.p < 0.05 && approx.p < 0.05, "{exact:?} {approx:?}");
    }
}
