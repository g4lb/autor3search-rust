fn main() {
    // Re-run when HEAD moves, so the recorded commit does not go stale.
    println!("cargo:rerun-if-changed=.git/HEAD");
    let commit = std::process::Command::new("git")
        .args(["describe", "--always", "--dirty", "--abbrev=7"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=AUTOR3SEARCH_GIT_COMMIT={commit}");
}
