use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_autor3search-rust")
}

#[test]
fn no_arguments_prints_usage_and_exits_64() {
    let out = Command::new(bin()).output().expect("run binary");
    assert_eq!(out.status.code(), Some(64), "no args must exit 64");
    let stderr = String::from_utf8_lossy(&out.stderr);
    for cmd in [
        "init", "doctor", "baseline", "profile", "eval", "status", "stop", "report", "version",
    ] {
        assert!(stderr.contains(cmd), "usage must list {cmd}: {stderr}");
    }
}

#[test]
fn unknown_command_exits_64() {
    let out = Command::new(bin())
        .arg("nope")
        .output()
        .expect("run binary");
    assert_eq!(out.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown command"));
}
