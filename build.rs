use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=.cargo_vcs_info.json");
    watch_git_head(Path::new(".git"));

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let commit = vcs_info_commit(Path::new(&manifest_dir))
        .or_else(git_describe)
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=AUTOR3SEARCH_GIT_COMMIT={commit}");
}

/// The commit cargo recorded when it packaged this crate.
///
/// `cargo package` writes `.cargo_vcs_info.json` into the archive, so this is
/// the only way a build from crates.io — where there is no git repository at
/// all — can name the commit it came from. It is tried first: the file exists
/// only in a published package, and when it does exist it is more trustworthy
/// than `git`, which would otherwise describe whatever repository happens to
/// enclose the vendored source.
fn vcs_info_commit(manifest_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(manifest_dir.join(".cargo_vcs_info.json")).ok()?;
    let sha = json_string_field(&text, "sha1")?;
    if sha.len() < 7 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(sha[..7].to_string())
}

/// The value of a string field in flat JSON, without taking a dependency on a
/// JSON parser for one field in one file.
fn json_string_field(text: &str, field: &str) -> Option<String> {
    let after_key = text.split_once(&format!("\"{field}\""))?.1;
    let after_colon = after_key.split_once(':')?.1;
    let open = after_colon.find('"')?;
    let rest = &after_colon[open + 1..];
    let close = rest.find('"')?;
    Some(rest[..close].to_string())
}

/// The commit as git sees it, for a build from a checkout: a tag when HEAD is
/// on one, an abbreviated hash otherwise, with `-dirty` on uncommitted work.
fn git_describe() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["describe", "--always", "--dirty", "--abbrev=7"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let described = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!described.is_empty()).then_some(described)
}

/// Asks cargo to re-run this script whenever HEAD moves.
///
/// Watching `.git/HEAD` alone is not enough, and the gap is easy to miss:
/// committing on the branch you are already on leaves that file byte-for-byte
/// identical — it holds `ref: refs/heads/<branch>`, not a hash — so the build
/// keeps reporting the commit before last. The ref HEAD names has to be
/// watched too, along with `packed-refs`, where a ref lives once git has
/// packed it and its loose file is gone.
fn watch_git_head(git: &Path) {
    // In a linked worktree `.git` is a file holding `gitdir: <path>`.
    let git_dir = match std::fs::read_to_string(git) {
        Ok(text) => match text.strip_prefix("gitdir:") {
            Some(path) => PathBuf::from(path.trim()),
            None => return,
        },
        Err(_) => git.to_path_buf(),
    };
    let head = git_dir.join("HEAD");
    println!("cargo:rerun-if-changed={}", head.display());
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("packed-refs").display()
    );
    if let Ok(contents) = std::fs::read_to_string(&head) {
        if let Some(reference) = contents.trim().strip_prefix("ref:") {
            println!(
                "cargo:rerun-if-changed={}",
                git_dir.join(reference.trim()).display()
            );
        }
    }
}
