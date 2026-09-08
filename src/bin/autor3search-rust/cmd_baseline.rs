use crate::args::Args;
use autor3search::{config, discover, freeze, gitx, results, scope, state};

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
    gitx::create_and_checkout_branch(&root, &branch)?;
    let commit = gitx::head_commit(&root)?;

    // Freeze the tests that live in their own files.
    let frozen = discover::frozen_test_files(&root, &cfg.unfreeze)?;
    let store = dir.join(freeze::STORE_DIR);
    let mut manifest = freeze::snapshot(&root, &store, &frozen).map_err(|e| e.to_string())?;

    // Hash the tests that cannot be: inline #[cfg(test)] modules and doctests
    // inside the source files the agent is allowed to edit.
    let matcher = scope::Matcher::new(&cfg.scope);
    for rel in discover::in_scope_sources(&root, &matcher)? {
        let text =
            std::fs::read_to_string(root.join(&rel)).map_err(|e| format!("read {rel}: {e}"))?;
        manifest
            .inline
            .insert(rel.clone(), freeze::inline_hashes(&text)?);
    }
    manifest.save(&dir.join(freeze::MANIFEST_PATH))?;

    // Pin the measurement worktree.
    let worktree = dir.join(state::WORKTREE_NAME);
    gitx::add_worktree(&root, &worktree, &commit)?;

    let record = state::Baseline {
        tag: tag.to_string(),
        branch: branch.clone(),
        commit: commit.clone(),
        measure_commit: commit.clone(),
        created_at: now_utc(),
        benchmarks: cfg.benchmarks.clone(),
        bench_targets: cfg.bench_targets.clone(),
        config_sha256: freeze::sha256_file(&cfg_path)?,
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
