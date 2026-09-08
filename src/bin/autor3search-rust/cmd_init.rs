use crate::args::Args;
use autor3search::{config, discover, gitx, runner};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

const PROGRAM_MD: &str = include_str!("../../../templates/program.md");

pub fn run(argv: &[String]) -> i32 {
    let args = match Args::parse(argv, &[("C", true), ("force", false)]) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return autor3search::EXIT_USAGE;
        }
    };
    match init(&args) {
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

fn init(args: &Args) -> Result<String, String> {
    let root = gitx::root(&args.dir())?;
    let cfg_path = root.join(config::CONFIG_PATH);
    if cfg_path.exists() && !args.flag("force") {
        return Err(format!(
            "{} already exists — pass --force to overwrite it",
            config::CONFIG_PATH
        ));
    }

    let targets = discover::bench_targets(&root)?;
    let criterion: Vec<_> = targets.into_iter().filter(|t| t.is_criterion).collect();

    let mut benchmarks = Vec::new();
    let mut best_effort = false;
    for t in &criterion {
        let r = runner::Runner::new(&root, Duration::from_secs(300));
        let out = r.cargo(
            &[
                "bench", "-p", &t.package, "--bench", &t.target, "--", "--list",
            ],
            None,
        )?;
        if out.ok() {
            let listed = discover::parse_list_output(&String::from_utf8_lossy(&out.stdout));
            benchmarks.extend(listed);
        } else {
            // The benchmarks do not currently build. Fall back to reading the
            // source, and say so: a criterion id can be computed at runtime,
            // and no static parse can be sure of those.
            best_effort = true;
            let src = std::fs::read_to_string(&t.src_path).unwrap_or_default();
            benchmarks.extend(discover::parse_benchmarks(&src));
        }
    }
    benchmarks.sort();
    benchmarks.dedup();

    if benchmarks.is_empty() {
        return Err(
            "no benchmarks found: this tool optimizes what it can measure, and refuses to \
             guess.\n\nAdd a criterion bench target to Cargo.toml:\n\n    \
             [[bench]]\n    name = \"my_bench\"\n    harness = false\n\nwrite a \
             c.bench_function(\"name\", ...) covering the code you actually want faster — \
             ideally the path that dominates your real workload, not a convenient helper — \
             then run init again."
                .to_string(),
        );
    }

    let cfg = config::Config {
        benchmarks: benchmarks.clone(),
        bench_targets: criterion
            .iter()
            .map(|t| config::BenchTarget {
                package: t.package.clone(),
                target: t.target.clone(),
            })
            .collect(),
        ..config::Config::default()
    };
    std::fs::create_dir_all(cfg_path.parent().unwrap())
        .map_err(|e| format!("create .autor3search: {e}"))?;
    std::fs::write(
        &cfg_path,
        serde_yaml::to_string(&cfg).map_err(|e| format!("encode config: {e}"))?,
    )
    .map_err(|e| format!("write config: {e}"))?;

    std::fs::write(root.join("program.md"), PROGRAM_MD)
        .map_err(|e| format!("write program.md: {e}"))?;
    write_gitignore(&root)?;

    let mut msg = String::new();
    msg.push_str(&format!("discovered {} benchmark(s):\n", benchmarks.len()));
    for b in &benchmarks {
        msg.push_str(&format!("  {b}\n"));
    }
    if best_effort {
        msg.push_str(
            "\nNOTE: some benchmarks could not be listed because the bench target does not \
             build, so the list above was parsed from source and is best-effort. Check it \
             against `cargo bench -- --list` once the benchmarks compile.\n",
        );
    }
    msg.push_str(&format!(
        "\nwrote {}\nwrote program.md\n\nNext:\n  git add -A && git commit -m \
         \"autor3search-rust init\"\n  autor3search-rust doctor\n  autor3search-rust \
         baseline --tag <today>\n",
        config::CONFIG_PATH
    ));
    Ok(msg)
}

/// Appends the harness's ignore rules, without disturbing existing ones.
///
/// `.autor3search/*` plus a negation, never `.autor3search/` wholesale:
/// ignoring the directory would leave `config.yaml` untracked, which
/// contradicts the entire reason it lives in the repository rather than the
/// state directory.
fn write_gitignore(root: &Path) -> Result<(), String> {
    const RULES: &[&str] = &[
        ".autor3search/*",
        "!.autor3search/config.yaml",
        "results.tsv",
        "run.log",
    ];
    let path = root.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let missing: Vec<&str> = RULES
        .iter()
        .filter(|r| !existing.lines().any(|l| l.trim() == **r))
        .copied()
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .map_err(|e| format!("open .gitignore: {e}"))?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(f).map_err(|e| format!("write .gitignore: {e}"))?;
    }
    writeln!(f, "\n# autor3search-rust").map_err(|e| format!("write .gitignore: {e}"))?;
    for rule in missing {
        writeln!(f, "{rule}").map_err(|e| format!("write .gitignore: {e}"))?;
    }
    Ok(())
}
