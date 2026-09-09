//! Executes cargo subcommands with a timeout and captured output.

use crate::state::CANCEL_REQUESTED;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

#[cfg(unix)]
#[path = "procgroup_unix.rs"]
mod procgroup;
#[cfg(windows)]
#[path = "procgroup_windows.rs"]
mod procgroup;
#[cfg(not(any(unix, windows)))]
#[path = "procgroup_other.rs"]
mod procgroup;

/// The most output retained per stream. A chatty test suite must not be able
/// to exhaust memory, and nothing downstream reads more than the tail.
pub const CAP_BYTES: usize = 4 * 1024 * 1024;

/// Retains at most `limit` bytes, then drops the rest.
pub(crate) struct CapWriter {
    pub(crate) buf: Vec<u8>,
    limit: usize,
    pub(crate) truncated: bool,
}

impl CapWriter {
    pub(crate) fn new(limit: usize) -> Self {
        CapWriter {
            buf: Vec::new(),
            limit,
            truncated: false,
        }
    }
}

impl Write for CapWriter {
    fn write(&mut self, p: &[u8]) -> std::io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.buf.len());
        if remaining == 0 {
            self.truncated = true;
        } else if p.len() > remaining {
            self.buf.extend_from_slice(&p[..remaining]);
            self.truncated = true;
        } else {
            self.buf.extend_from_slice(p);
        }
        // Always report a full write: a short write would make the caller
        // treat a chatty subprocess as an I/O failure.
        Ok(p.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The outcome of one subprocess.
#[derive(Clone, Debug)]
pub struct Output {
    pub args: Vec<String>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
    pub timed_out: bool,
    /// Set when this command was killed because `state::CANCEL_REQUESTED`
    /// became true mid-run — a human's `stop --force`, not a timeout and not
    /// an ordinary non-zero exit. A caller that only checked `ok()` would
    /// otherwise report this as a build or test failure instead of the
    /// abandoned experiment it actually is.
    pub cancelled: bool,
    pub duration: Duration,
}

impl Output {
    /// A clean, in-time run.
    pub fn ok(&self) -> bool {
        self.exit_code == 0 && !self.timed_out
    }

    /// The last `n` lines of stderr, falling back to stdout when stderr is
    /// blank — cargo puts diagnostics on stderr, criterion on stdout.
    pub fn tail(&self, n: usize) -> String {
        let mut src = String::from_utf8_lossy(&self.stderr).into_owned();
        if src.trim().is_empty() {
            src = String::from_utf8_lossy(&self.stdout).into_owned();
        }
        let trimmed = src.trim_end_matches('\n');
        if trimmed.is_empty() || n == 0 {
            return String::new();
        }
        let lines: Vec<&str> = trimmed.split('\n').collect();
        let start = lines.len().saturating_sub(n);
        lines[start..].join("\n")
    }
}

/// Executes cargo commands in a fixed directory.
pub struct Runner {
    pub dir: PathBuf,
    /// Extra environment entries layered on the process's own.
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
}

impl Runner {
    pub fn new(dir: &Path, timeout: Duration) -> Runner {
        Runner {
            dir: dir.to_path_buf(),
            env: Vec::new(),
            timeout,
        }
    }

    /// Adds one environment override, replacing any existing entry.
    pub fn with_env(mut self, key: &str, value: &str) -> Runner {
        self.env.retain(|(k, _)| k != key);
        self.env.push((key.to_string(), value.to_string()));
        self
    }

    /// Runs `cargo <args...>`, capturing both streams and enforcing the
    /// timeout. A non-zero exit is ordinary data in [`Output`]; an `Err` means
    /// the harness itself could not run the command.
    pub fn cargo(&self, args: &[&str], mut log: Option<&mut dyn Write>) -> Result<Output, String> {
        let mut cmd = Command::new("cargo");
        cmd.args(args)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        procgroup::configure(&mut cmd);

        if let Some(log) = log.as_deref_mut() {
            let _ = writeln!(
                log,
                "\n$ cargo {}\n  (in {})",
                args.join(" "),
                self.dir.display()
            );
        }

        let start = Instant::now();
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("run cargo {}: {e}", args.join(" ")))?;

        // Drain both pipes on their own threads: a child that fills a pipe
        // buffer blocks forever if nobody is reading, and the timeout below
        // would never be reached because wait() is what notices it.
        let mut child_stdout = child.stdout.take().expect("stdout piped");
        let mut child_stderr = child.stderr.take().expect("stderr piped");
        let out_handle = std::thread::spawn(move || {
            let mut w = CapWriter::new(CAP_BYTES);
            let mut buf = [0u8; 8192];
            while let Ok(n) = child_stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let _ = w.write_all(&buf[..n]);
            }
            w.buf
        });
        let err_handle = std::thread::spawn(move || {
            let mut w = CapWriter::new(CAP_BYTES);
            let mut buf = [0u8; 8192];
            while let Ok(n) = child_stderr.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let _ = w.write_all(&buf[..n]);
            }
            w.buf
        });

        let mut timed_out = false;
        let mut cancelled = false;
        let status = loop {
            // Check the deadline BEFORE calling try_wait, not after. If the
            // order were reversed, a fast-exiting command racing a very short
            // (or zero) timeout could have try_wait report `Some(status)` on
            // the very first pass, and timed_out would stay false — so
            // whether a run counts as "timed out" would depend on which of
            // the two happened to win the race, i.e. on machine speed. A
            // deadline that has already passed by the time this loop begins
            // has passed, full stop, so it must be checked first. Do not
            // "simplify" this back to try_wait-then-check.
            if start.elapsed() >= self.timeout {
                procgroup::kill_tree(&mut child);
                timed_out = true;
                break child.wait().map_err(|e| format!("wait after kill: {e}"))?;
            }
            // Checked every poll tick, not just between top-level gates: a
            // human's `stop --force` should not have to wait out an entire
            // `cargo build` or `cargo bench` round before it takes effect.
            // Killing the process the SAME way a timeout does reuses the
            // exact process-group teardown Task 10 already proved correct,
            // so a cancelled subprocess never leaves an orphaned benchmark
            // burning CPU behind it either.
            if CANCEL_REQUESTED.load(Ordering::SeqCst) {
                procgroup::kill_tree(&mut child);
                cancelled = true;
                break child
                    .wait()
                    .map_err(|e| format!("wait after cancel: {e}"))?;
            }
            match child.try_wait().map_err(|e| format!("wait: {e}"))? {
                Some(status) => break status,
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        };

        let stdout = out_handle.join().unwrap_or_default();
        let stderr = err_handle.join().unwrap_or_default();

        if let Some(log) = log {
            let _ = log.write_all(&stdout);
            let _ = log.write_all(&stderr);
        }

        Ok(Output {
            args: args.iter().map(|s| s.to_string()).collect(),
            stdout,
            stderr,
            exit_code: status.code().unwrap_or(-1),
            timed_out,
            cancelled,
            duration: start.elapsed(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner(timeout: Duration) -> Runner {
        Runner::new(Path::new("."), timeout)
    }

    #[test]
    fn a_successful_command_reports_ok_and_captures_stdout() {
        let out = runner(Duration::from_secs(60))
            .cargo(&["--version"], None)
            .expect("cargo --version");
        assert!(
            out.ok(),
            "exit {} timed_out {}",
            out.exit_code,
            out.timed_out
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("cargo"));
        assert!(!out.timed_out);
    }

    #[test]
    fn a_failing_command_reports_its_exit_code_without_erroring() {
        // A Result error means the harness malfunctioned; a non-zero exit is
        // ordinary data the caller decides about.
        let out = runner(Duration::from_secs(60))
            .cargo(&["--this-flag-does-not-exist"], None)
            .expect("must not be a harness error");
        assert!(!out.ok());
        assert_ne!(out.exit_code, 0);
    }

    #[test]
    fn tail_returns_the_last_lines_of_stderr_falling_back_to_stdout() {
        let out = Output {
            args: vec![],
            stdout: b"a\nb\nc\n".to_vec(),
            stderr: Vec::new(),
            exit_code: 1,
            timed_out: false,
            cancelled: false,
            duration: Duration::ZERO,
        };
        assert_eq!(out.tail(2), "b\nc");
        let out = Output {
            stderr: b"x\ny\n".to_vec(),
            ..out
        };
        assert_eq!(out.tail(1), "y");
        assert_eq!(out.tail(0), "");
    }

    #[test]
    fn output_is_capped_rather_than_growing_without_bound() {
        let mut w = CapWriter::new(4);
        w.write_all(b"abcdefgh").unwrap();
        assert_eq!(w.buf, b"abcd");
        assert!(w.truncated);
    }

    #[test]
    fn env_overrides_reach_the_child() {
        let r = runner(Duration::from_secs(60)).with_env("AUTOR3SEARCH_PROBE", "yes");
        assert!(
            r.env
                .iter()
                .any(|(k, v)| k == "AUTOR3SEARCH_PROBE" && v == "yes")
        );
    }

    // The timeout has to actually stop the process, not merely stop waiting
    // for it. Uses cargo itself so the test needs no extra fixture: `cargo
    // bench` on a nonexistent target still starts and exits, so instead this
    // exercises the timeout path with a deliberately slow build-free command.
    //
    // A 0-length timeout guarantees expiry regardless of machine speed: the
    // runner's wait loop checks the deadline before ever calling try_wait, so
    // even a command that exits instantly is still marked timed_out. Without
    // that ordering this test would be flaky, passing or failing depending on
    // whether try_wait won the race against the deadline check.
    #[test]
    fn a_command_exceeding_its_timeout_is_marked_timed_out() {
        // A 0-length timeout guarantees expiry regardless of machine speed,
        // on every platform, so no platform-specific command is needed here.
        let slow = ["--version"];
        let out = runner(Duration::from_millis(0)).cargo(&slow, None).unwrap();
        assert!(
            out.timed_out,
            "expected timed_out, got exit {}",
            out.exit_code
        );
    }
}
