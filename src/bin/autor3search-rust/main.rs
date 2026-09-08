use std::process::ExitCode;

mod args;
mod cmd_baseline;
mod cmd_init;

const COMMANDS: &[(&str, &str)] = &[
    (
        "init",
        "scan repo, discover benchmarks, write config and program.md",
    ),
    ("doctor", "check whether this machine can measure reliably"),
    (
        "baseline",
        "create the run branch, freeze tests, record the baseline",
    ),
    (
        "profile",
        "profile the declared benchmarks and report hot spots",
    ),
    ("eval", "run one experiment step and return a verdict"),
    (
        "status",
        "show where the run is: branch, worktree, experiments, stop state",
    ),
    (
        "stop",
        "ask the agent to end the run after the current experiment",
    ),
    ("report", "summarize results.tsv"),
    ("version", "print which build of the harness this is"),
];

fn usage() {
    eprintln!("autor3search-rust — autonomous Rust performance optimization harness");
    eprintln!("\nusage: autor3search-rust <command> [flags]\n\ncommands:");
    for (name, summary) in COMMANDS {
        eprintln!("  {name:<9} {summary}");
    }
}

fn dispatch(args: &[String]) -> i32 {
    let Some(name) = args.first() else {
        usage();
        return autor3search::EXIT_USAGE;
    };
    if !COMMANDS.iter().any(|(c, _)| c == name) {
        eprintln!("unknown command {name:?}\n");
        usage();
        return autor3search::EXIT_USAGE;
    }
    let rest = &args[1..];
    match name.as_str() {
        "init" => cmd_init::run(rest),
        "baseline" => cmd_baseline::run(rest),
        _ => {
            eprintln!("{name}: not implemented yet");
            autor3search::EXIT_USAGE
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(dispatch(&args) as u8)
}
