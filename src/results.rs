//! Reads and appends the experiment log.

use std::io::Write;
use std::path::Path;

/// The log location relative to the repository root.
pub const RESULTS_PATH: &str = "results.tsv";

/// The first line of every log file.
///
/// Five columns, where the Go tool has six: criterion measures time only, so
/// there is no `allocs_delta` to record. Keeping an always-empty column would
/// be worse than dropping it.
pub const HEADER: &str = "commit\tscore\tbest_bench_delta\tstatus\tdescription";

/// The longest description written verbatim.
///
/// `--desc` has no length cap of its own, and an agent pasting something large
/// would otherwise produce a line long enough to be a nuisance to every future
/// read. 256 characters is generous for a one-line experiment summary.
pub const MAX_DESCRIPTION_LEN: usize = 256;

/// One logged experiment. Field order must stay in step with [`HEADER`],
/// [`append`]'s format string, and [`load`]'s indexing.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub commit: String,
    /// The geomean of per-benchmark ratios; 1.0 means no change.
    pub score: f64,
    /// The largest single-benchmark improvement, percent.
    pub best_bench_delta: f64,
    pub status: String,
    pub description: String,
}

/// Makes a field safe for a tab-separated single-line record.
fn clean(s: &str) -> String {
    s.replace(['\t', '\r', '\n'], " ").trim().to_string()
}

/// Caps at [`MAX_DESCRIPTION_LEN`] **characters**, appending an ellipsis when
/// it had to cut. Counting characters rather than bytes means a multi-byte
/// character is never split in half into invalid UTF-8.
fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_DESCRIPTION_LEN {
        return s.to_string();
    }
    let kept: String = s.chars().take(MAX_DESCRIPTION_LEN).collect();
    format!("{kept}...")
}

/// Adds one row, creating the file with a header when needed.
pub fn append(path: &Path, row: &Row) -> Result<(), String> {
    let is_new = !path.exists();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
    }
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    if is_new {
        writeln!(f, "{HEADER}").map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    writeln!(
        f,
        "{}\t{:.4}\t{:.2}\t{}\t{}",
        clean(&row.commit),
        row.score,
        row.best_bench_delta,
        clean(&row.status),
        truncate(&clean(&row.description))
    )
    .map_err(|e| format!("write {}: {e}", path.display()))?;
    // Durability matters: this is the sole record of an overnight run.
    f.sync_all()
        .map_err(|e| format!("sync {}: {e}", path.display()))?;
    Ok(())
}

/// Reads every row. A missing file is an empty log, not an error.
///
/// Strict by design: a malformed line fails the whole load rather than being
/// skipped. Sanitising on write makes a malformed row nearly impossible, so
/// one is a real signal — a torn write or a hand edit — and the error names
/// the file and line so it can be fixed. Silently dropping rows would let a
/// corrupted log masquerade as a short one, which is worse for a file that is
/// the sole record of an overnight run.
pub fn load(path: &Path) -> Result<Vec<Row>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("open {}: {e}", path.display())),
    };
    let mut rows = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line_no = i + 1;
        if line.is_empty() || line == HEADER {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() != 5 {
            return Err(format!(
                "{}:{line_no}: got {} fields, want 5",
                path.display(),
                parts.len()
            ));
        }
        let score: f64 = parts[1]
            .parse()
            .map_err(|e| format!("{}:{line_no}: score: {e}", path.display()))?;
        let best: f64 = parts[2]
            .parse()
            .map_err(|e| format!("{}:{line_no}: best_bench_delta: {e}", path.display()))?;
        rows.push(Row {
            commit: parts[0].to_string(),
            score,
            best_bench_delta: best,
            status: parts[3].to_string(),
            description: parts[4].to_string(),
        });
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(status: &str, score: f64) -> Row {
        Row {
            commit: "abc1234".into(),
            score,
            best_bench_delta: -12.5,
            status: status.into(),
            description: "tried a thing".into(),
        }
    }

    fn tmp() -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("results.tsv");
        (d, p)
    }

    #[test]
    fn a_missing_file_is_an_empty_log_not_an_error() {
        let (_d, p) = tmp();
        assert!(load(&p).unwrap().is_empty());
    }

    #[test]
    fn the_first_append_writes_the_header_and_one_row() {
        let (_d, p) = tmp();
        append(&p, &row("KEEP", 0.87)).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with(HEADER), "{text}");
        assert_eq!(text.lines().count(), 2);
        assert_eq!(text.lines().next().unwrap().split('\t').count(), 5);
    }

    #[test]
    fn rows_round_trip() {
        let (_d, p) = tmp();
        append(&p, &row("KEEP", 0.87)).unwrap();
        append(&p, &row("DISCARD", 1.01)).unwrap();
        let rows = load(&p).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].status, "KEEP");
        assert!((rows[0].score - 0.87).abs() < 1e-9);
        assert_eq!(rows[1].status, "DISCARD");
        assert_eq!(rows[0].description, "tried a thing");
    }

    // Tabs and newlines in a description would produce a row that no longer
    // parses as one record.
    #[test]
    fn control_characters_are_flattened_to_spaces() {
        let (_d, p) = tmp();
        let mut r = row("FAIL", 1.0);
        r.description = "line one\nline\ttwo\r\n".into();
        append(&p, &r).unwrap();
        let rows = load(&p).unwrap();
        assert_eq!(rows[0].description, "line one line two");
    }

    // An agent pasting a stack trace into --desc must not be able to produce a
    // line long enough to jam every future load.
    #[test]
    fn an_over_long_description_is_truncated_with_an_ellipsis() {
        let (_d, p) = tmp();
        let mut r = row("KEEP", 0.9);
        r.description = "x".repeat(MAX_DESCRIPTION_LEN + 50);
        append(&p, &r).unwrap();
        let rows = load(&p).unwrap();
        assert_eq!(rows[0].description.chars().count(), MAX_DESCRIPTION_LEN + 3);
        assert!(rows[0].description.ends_with("..."));
    }

    // Truncating by character, not byte, so a multi-byte character is never
    // split in half into invalid UTF-8.
    #[test]
    fn truncation_does_not_split_a_multibyte_character() {
        let (_d, p) = tmp();
        let mut r = row("KEEP", 0.9);
        r.description = "é".repeat(MAX_DESCRIPTION_LEN + 50);
        append(&p, &r).unwrap();
        assert!(load(&p).is_ok());
    }

    // Strict by design: a malformed line is a torn write or a hand edit, and
    // silently dropping rows would let a corrupted log masquerade as a short
    // one — worse for a file that is the sole record of an overnight run.
    #[test]
    fn a_malformed_line_fails_the_load_and_names_the_line() {
        let (_d, p) = tmp();
        append(&p, &row("KEEP", 0.9)).unwrap();
        let mut text = std::fs::read_to_string(&p).unwrap();
        text.push_str("not\tenough\tfields\n");
        std::fs::write(&p, text).unwrap();
        let err = load(&p).unwrap_err();
        assert!(err.contains(":3"), "must name the line: {err}");
        assert!(err.contains("want 5"), "{err}");
    }

    #[test]
    fn an_unparseable_number_fails_the_load() {
        let (_d, p) = tmp();
        std::fs::write(&p, format!("{HEADER}\nabc\tnotanumber\t0.0\tKEEP\tx\n")).unwrap();
        assert!(load(&p).unwrap_err().contains("score"));
    }

    #[test]
    fn blank_lines_and_repeated_headers_are_skipped() {
        let (_d, p) = tmp();
        std::fs::write(&p, format!("{HEADER}\n\nabc\t0.9\t-1.0\tKEEP\tx\n\n")).unwrap();
        assert_eq!(load(&p).unwrap().len(), 1);
    }
}
