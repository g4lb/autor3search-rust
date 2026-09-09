use crate::args::Args;
use autor3search::{config, discover, freeze, gitx, results, scope, state};
use std::path::Path;

pub fn run(argv: &[String]) -> i32 {
    let args = match Args::parse(argv, &[("C", true), ("tag", true), ("force", false)]) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return autor3search::EXIT_USAGE;
        }
    };
    match baseline(&args) {
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

fn baseline(args: &Args) -> Result<String, String> {
    let tag = args
        .value("tag")
        .ok_or("--tag is required, e.g. --tag sep8")?;
    state::valid_tag(tag)?;

    let root = gitx::root(&args.dir())?;
    let cfg_path = root.join(config::CONFIG_PATH);
    if !cfg_path.exists() {
        return Err(format!(
            "no {} — run 'autor3search-rust init' first",
            config::CONFIG_PATH
        ));
    }
    let cfg = config::Config::load(&cfg_path)?;

    // A baseline pinned against what is on disk rather than what is in git
    // would not be reproducible, so the tree must be committed first.
    if !gitx::is_clean(&root)? {
        return Err(
            "the working tree has uncommitted changes — commit them first:\n    \
             git add -A && git commit -m \"autor3search-rust init\"\n\nA baseline pinned \
             against what is on disk rather than what is in git would not be reproducible."
                .to_string(),
        );
    }

    let dir = state::state_dir(&root, tag)?;
    if dir.join(state::BASELINE_FILE).exists() {
        return Err(format!(
            "tag {tag:?} already has a baseline at {} — pick a new tag, or delete that \
             directory to start over",
            dir.display()
        ));
    }
    // A results.tsv holding rows from an earlier run would make `report`
    // mix two runs' experiments into one cumulative figure.
    let results_path = root.join(results::RESULTS_PATH);
    if !results::load(&results_path)?.is_empty() && !args.flag("force") {
        return Err(format!(
            "{} already holds experiment rows from an earlier run — move it aside, or pass \
             --force to start a new run against it",
            results::RESULTS_PATH
        ));
    }

    let branch = state::branch_name(tag);
    if gitx::branch_exists(&root, &branch)? {
        return Err(format!("branch {branch} already exists — pick a new tag"));
    }

    // Nothing above this line has mutated the repository or written into the
    // state directory, so nothing above needs a rollback. Everything below
    // does, and `baseline`'s whole contract is doing it exactly once: a
    // failure partway through must not leave wreckage that a retry misreads
    // as a naming collision (the branch or the state directory already
    // existing) rather than the real problem.
    let original_branch = gitx::current_branch(&root)?;
    gitx::create_and_checkout_branch(&root, &branch)?;

    freeze_and_pin(&root, &dir, &cfg, &cfg_path, tag, &branch)
        .map_err(|e| rollback(&root, &dir, &branch, &original_branch, e))
}

/// Everything `baseline` does once the run branch exists: freezing the
/// tests that live in their own files, hashing the inline tests that
/// cannot be, pinning the measurement worktree, and recording the baseline.
///
/// Split out from [`baseline`] so its errors can be rolled back as a single
/// unit by the caller — this function itself never cleans up after its own
/// failure.
fn freeze_and_pin(
    root: &Path,
    dir: &Path,
    cfg: &config::Config,
    cfg_path: &Path,
    tag: &str,
    branch: &str,
) -> Result<String, String> {
    let commit = gitx::head_commit(root)?;

    // Freeze the tests that live in their own files.
    let frozen = discover::frozen_test_files(root, &cfg.unfreeze)?;
    let store = dir.join(freeze::STORE_DIR);
    let mut manifest = freeze::snapshot(root, &store, &frozen).map_err(|e| e.to_string())?;

    // Hash the tests that cannot be: inline #[cfg(test)] modules and doctests
    // inside the source files the agent is allowed to edit. `inline_hashes_at`
    // (not the pure-string `inline_hashes`) is what lets a `#[cfg(test)] mod
    // tests;` declared without a body resolve to its own file.
    let matcher = scope::Matcher::new(&cfg.scope);
    for rel in discover::in_scope_sources(root, &matcher)? {
        let hashes = freeze::inline_hashes_at(root, &rel).map_err(|e| e.to_string())?;
        manifest.inline.insert(rel.clone(), hashes);
    }
    manifest.save(&dir.join(freeze::MANIFEST_PATH))?;

    // Hash every locked file present on disk, independent of git: `eval`
    // gate 2 stats these paths directly, which is what catches a locked file
    // that a repo's own (or the agent's own) `.gitignore` hides from
    // `git diff`/`git status`.
    let mut locked_files = std::collections::BTreeMap::new();
    for rel in discover::locked_files(root)? {
        locked_files.insert(rel.clone(), freeze::sha256_file(&root.join(&rel))?);
    }

    // Pin the measurement worktree.
    let worktree = dir.join(state::WORKTREE_NAME);
    gitx::add_worktree(root, &worktree, &commit)?;

    let record = state::Baseline {
        tag: tag.to_string(),
        branch: branch.to_string(),
        commit: commit.clone(),
        measure_commit: commit.clone(),
        created_at: now_utc(),
        benchmarks: cfg.benchmarks.clone(),
        bench_targets: cfg.bench_targets.clone(),
        config_sha256: freeze::sha256_file(cfg_path)?,
        locked_files,
    };
    record.save(&dir.join(state::BASELINE_FILE))?;

    Ok(format!(
        "run tag        {tag}\nbranch         {branch}  (checked out)\nbaseline       \
         {commit}\nfrozen         {} test file(s), {} source file(s) hashed for inline \
         tests\nworktree       {}\nbenchmarks     {}\n\nStart your agent in this repository \
         and point it at program.md.\n",
        manifest.files.len(),
        manifest.inline.len(),
        worktree.display(),
        record.benchmarks.join(", ")
    ))
}

/// Undoes everything [`freeze_and_pin`] may have partially done: checks the
/// repository back out onto `original_branch`, force-deletes the abandoned
/// run branch, and removes the partial state directory. Returns
/// `original_err`, with any trouble the rollback itself hits appended as
/// additional context.
///
/// The rollback's own failure must never REPLACE `original_err` — whatever
/// actually broke the run is what the person retrying needs to see first,
/// not a secondary complaint about cleanup.
fn rollback(
    root: &Path,
    dir: &Path,
    branch: &str,
    original_branch: &str,
    original_err: String,
) -> String {
    let mut msg = original_err;
    if let Err(e) = gitx::checkout_branch(root, original_branch) {
        msg.push_str(&format!(
            "\n\nadditionally, failed to check the repository back out onto {original_branch:?} \
             after this failure: {e}\n    to recover by hand: git checkout {original_branch}"
        ));
    }
    if let Err(e) = gitx::delete_branch(root, branch) {
        msg.push_str(&format!(
            "\n\nadditionally, failed to delete the abandoned run branch {branch:?}: {e}\n    \
             to recover by hand: git branch -D {branch}"
        ));
    }
    if dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(dir) {
            msg.push_str(&format!(
                "\n\nadditionally, failed to remove the partial state directory {}: {e}\n    \
                 to recover by hand: rm -rf {}",
                dir.display(),
                dir.display()
            ));
        }
    }
    msg
}

/// An RFC3339 UTC timestamp, without pulling in a date library for one field.
fn now_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Civil-from-days, Howard Hinnant's algorithm.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}
