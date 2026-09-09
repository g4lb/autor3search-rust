//! The `report` command: summarizes `results.tsv`.

use crate::args::Args;
use autor3search::gitx;
use autor3search::results::{self, Row};

pub fn run(argv: &[String]) -> i32 {
    let args = match Args::parse(argv, &[("C", true)]) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return autor3search::EXIT_USAGE;
        }
    };
    match report(&args) {
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

fn report(args: &Args) -> Result<String, String> {
    let root = gitx::root(&args.dir())?;
    let rows = results::load(&root.join(results::RESULTS_PATH))?;
    Ok(format_report(&rows))
}

/// Renders the summary. The cumulative speedup is the PRODUCT of every kept
/// row's score, not the latest.
///
/// Each kept score is only that experiment's own incremental contribution,
/// because `measure_commit` advances after every KEEP (see
/// `autor3search::pipeline::advance_measurement_baseline`): every `eval`
/// measures the candidate against the immediately preceding ACCEPTED state,
/// never against the run's original commit. So the run-level figure has to
/// be composed by multiplying every kept score together, the same way
/// compounding successive percentage changes works — taking the latest
/// alone would under-report every earlier kept experiment.
fn format_report(rows: &[Row]) -> String {
    if rows.is_empty() {
        return "autor3search-rust report: no experiments recorded\n".to_string();
    }

    let mut keep = 0;
    let mut discard = 0;
    let mut fail = 0;
    let mut crash = 0;
    for r in rows {
        match r.status.as_str() {
            "KEEP" => keep += 1,
            "DISCARD" => discard += 1,
            "FAIL" => fail += 1,
            "CRASH" => crash += 1,
            _ => {}
        }
    }

    let mut cumulative = 1.0f64;
    let mut kept_rows: Vec<&Row> = Vec::new();
    for r in rows {
        if r.status == "KEEP" {
            cumulative *= r.score;
            kept_rows.push(r);
        }
    }

    let mut out = format!(
        "autor3search-rust report: {} experiment(s)  ({keep} keep, {discard} discard, {fail} \
         fail, {crash} crash)\n\n",
        rows.len()
    );

    if kept_rows.is_empty() {
        out.push_str("cumulative speedup: no experiments were kept\n");
    } else {
        let speedup_pct = (1.0 - cumulative) * 100.0;
        let multiplier = if cumulative > 0.0 {
            1.0 / cumulative
        } else {
            f64::INFINITY
        };
        out.push_str(&format!(
            "cumulative speedup: {speedup_pct:.1}%  (score {cumulative:.2}, {multiplier:.2}x \
             faster overall)\n"
        ));

        // Largest individual wins: every kept row, sorted by its own
        // best-benchmark delta (most negative — the biggest improvement —
        // first).
        let mut sorted = kept_rows.clone();
        sorted.sort_by(|a, b| {
            a.best_bench_delta
                .partial_cmp(&b.best_bench_delta)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.push_str("\nlargest wins:\n");
        for (i, r) in sorted.iter().take(5).enumerate() {
            let desc = if r.description.is_empty() {
                "(no description)"
            } else {
                &r.description
            };
            out.push_str(&format!(
                "  {}. {desc} (time: {:+.1}%)\n",
                i + 1,
                r.best_bench_delta
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(status: &str, score: f64, best_bench_delta: f64, desc: &str) -> Row {
        Row {
            commit: "abc1234".into(),
            score,
            best_bench_delta,
            status: status.into(),
            description: desc.into(),
        }
    }

    #[test]
    fn an_empty_log_says_so() {
        assert!(format_report(&[]).contains("no experiments recorded"));
    }

    #[test]
    fn counts_and_the_product_of_kept_scores_are_reported() {
        let rows = vec![
            row("KEEP", 0.8, -20.0, "a"),
            row("KEEP", 0.5, -50.0, "b"),
            row("DISCARD", 1.0, 0.0, "c"),
            row("FAIL", 0.0, 0.0, "d"),
        ];
        let text = format_report(&rows);
        assert!(text.contains("2 keep"), "{text}");
        assert!(text.contains("1 discard"), "{text}");
        assert!(text.contains("1 fail"), "{text}");
        // 0.8 * 0.5 = 0.40, the PRODUCT rather than the latest kept score
        // (0.5) alone.
        assert!(text.contains("0.40"), "{text}");
    }

    #[test]
    fn no_kept_rows_says_so_rather_than_zero() {
        let rows = vec![row("DISCARD", 1.0, 0.0, "c")];
        let text = format_report(&rows);
        assert!(!text.contains("0.0%"), "{text}");
        assert!(
            text.to_lowercase().contains("no experiments were kept"),
            "{text}"
        );
    }
}
