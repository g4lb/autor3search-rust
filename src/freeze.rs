//! Snapshots test files at baseline and restores them before every
//! evaluation, so an agent cannot weaken its own success criteria.
//!
//! Paths here are relative to the run's out-of-tree state directory, never to
//! the repository root: the frozen store and its manifest are part of what the
//! metric depends on, so they must live where the agent being measured cannot
//! reach them.

use quote::ToTokens;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

/// The frozen store, relative to the state directory.
pub const STORE_DIR: &str = "frozen";
/// The manifest, relative to the state directory.
pub const MANIFEST_PATH: &str = "frozen/manifest.json";

/// Hashes of the test material that cannot be frozen as a file, per in-scope
/// source file. Populated by [`crate::freeze::inline_hashes`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineHashes {
    /// Over the token text of every item gated by a `test`-mentioning `cfg`
    /// (most commonly a whole `#[cfg(test)] mod tests { ... }`, but any item
    /// kind that can carry the attribute counts).
    pub cfg_test: String,
    /// Over every doc-comment line, which is a doctest's whole content.
    pub doc: String,
    /// SHA-256 of the raw bytes of every repo-relative file this hash's
    /// computation actually depended on: the file itself, plus every
    /// out-of-line module file recursively reached from it (see
    /// [`inline_hashes_at`]). Used only to short-circuit `verify_inline`'s
    /// re-parse: the parse is a pure function of exactly this content, so
    /// identical bytes everywhere in this set guarantee an identical
    /// `cfg_test`/`doc`, and a change anywhere in it is caught by falling
    /// back to a real re-parse rather than by trusting this map's shape.
    ///
    /// `None` for a manifest saved before this field existed, or for a hash
    /// computed by the pure-string [`inline_hashes`] (there is no
    /// filesystem to hash against) — always treated as "unknown, must
    /// parse", never as "unchanged", so an old manifest is never silently
    /// exempted from the real check. Deliberately excluded from this type's
    /// derived equality's *use* in `verify_inline` (only `cfg_test`/`doc`
    /// are compared there): reformatting an out-of-line module changes its
    /// bytes without changing its token-stream hash, and this field must
    /// never turn that into a false positive.
    #[serde(default)]
    pub content_sha256: Option<BTreeMap<String, String>>,
}

/// Maps repo-relative paths to their hash at baseline time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Frozen whole files: `tests/**` and `benches/**`.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// In-scope source files' inline test hashes.
    #[serde(default)]
    pub inline: BTreeMap<String, InlineHashes>,
}

impl Manifest {
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let mut json =
            serde_json::to_string_pretty(self).map_err(|e| format!("encode manifest: {e}"))?;
        json.push('\n');
        std::fs::write(path, json).map_err(|e| format!("write {}: {e}", path.display()))
    }

    pub fn load(path: &Path) -> Result<Manifest, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("read manifest {}: {e}", path.display()))?;
        serde_json::from_str(&raw).map_err(|e| format!("parse manifest {}: {e}", path.display()))
    }
}

/// What can go wrong while freezing. The three variants are distinguished
/// because [`crate::pipeline`] reports two of them as tampering — a `FAIL`
/// verdict with an actionable message — rather than as a harness malfunction.
#[derive(Debug)]
pub enum FreezeError {
    Io(String),
    /// A symlink somewhere along a frozen file's path. Reads and writes follow
    /// links, so writing to a symlinked destination writes *through* it,
    /// potentially outside the repository entirely.
    Symlink(String),
    /// A golden copy no longer hashes to what the manifest recorded.
    StoreTampered(String),
}

impl std::fmt::Display for FreezeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FreezeError::Io(m) | FreezeError::Symlink(m) | FreezeError::StoreTampered(m) => {
                write!(f, "{m}")
            }
        }
    }
}

impl std::error::Error for FreezeError {}

/// Hex-encoded SHA-256 of some bytes.
pub fn sha256_bytes(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    format!("{:x}", h.finalize())
}

/// Hex-encoded SHA-256 of a file's contents.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(sha256_bytes(&bytes))
}

/// Joins `rel` onto `root`, rejecting anything that would escape it.
///
/// Manifest entries come from a JSON file on disk, so they are untrusted
/// input: `restore` writes through them before every evaluation.
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, FreezeError> {
    // Rust's non-Windows `Path` parser never treats `\` as a separator, so a
    // backslash-separated escape (or drive prefix) is not absolute and has no
    // `..` component here — it is just one literal filename. Reject it
    // structurally, on every platform, so a manifest written on Windows and
    // read on Unix (or vice versa) is refused identically rather than one
    // side silently letting it through only to fail downstream with a
    // confusing "no such file".
    if rel.contains('\\') || rel.contains(':') {
        return Err(FreezeError::Io(format!(
            "frozen path {rel:?} must use forward slashes and no drive prefix"
        )));
    }
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(FreezeError::Io(format!(
            "frozen path {rel:?} must be relative"
        )));
    }
    // Reject any climb, on every platform, so the check means the same thing
    // everywhere.
    for c in rel_path.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(FreezeError::Io(format!(
                    "frozen path {rel:?} escapes the repository root"
                )));
            }
        }
    }
    Ok(root.join(rel_path))
}

/// The first component of `rel`, beneath `root`, that is a symlink — or
/// `None` when none is.
///
/// `root` itself is deliberately not examined: a repository legitimately
/// reached through a symlinked ancestor (macOS's `/tmp`, a home directory on a
/// linked volume) is not tampering, and refusing to work there would break
/// ordinary setups.
///
/// `pub(crate)` rather than private: [`crate::pipeline`]'s scope/locked-file
/// gate reuses this exact check on every changed path before matching it
/// against [`crate::scope::locked_file`] or the scope [`crate::scope::Matcher`]
/// — a symlinked path (or one reached through a symlinked ancestor) can make
/// `git status` report a single innocuous-looking entry (e.g. `.cargo`) that
/// actually resolves, at build time, to a locked file such as
/// `.cargo/config.toml`. One implementation means one place to get the
/// "root itself is exempt" exception right.
pub(crate) fn symlink_component(root: &Path, rel: &str) -> Result<Option<String>, FreezeError> {
    let mut path = root.to_path_buf();
    let mut seen: Vec<String> = Vec::new();
    for part in Path::new(rel).components() {
        let Component::Normal(part) = part else {
            continue;
        };
        path.push(part);
        seen.push(part.to_string_lossy().into_owned());
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => return Ok(Some(seen.join("/"))),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(FreezeError::Io(format!("stat {}: {e}", path.display()))),
        }
    }
    Ok(None)
}

fn no_symlink(root: &Path, rel: &str, what: &str) -> Result<(), FreezeError> {
    if let Some(link) = symlink_component(root, rel)? {
        return Err(FreezeError::Symlink(format!(
            "{what} {rel}: {link} is a symlink; refusing to read or write through it, which \
             could reach a file outside the repository"
        )));
    }
    Ok(())
}

/// Copies each file into `store_dir` and records its hash. Repo-relative
/// paths are preserved inside the store.
pub fn snapshot(
    repo_root: &Path,
    store_dir: &Path,
    files: &[String],
) -> Result<Manifest, FreezeError> {
    let mut m = Manifest::default();
    for rel in files {
        no_symlink(repo_root, rel, "snapshot")?;
        // A directory left by an earlier attempt under the same tag is not
        // necessarily pristine: refuse to write the golden copy through a link
        // there either.
        no_symlink(store_dir, rel, "snapshot (store)")?;
        let src = safe_join(repo_root, rel)?;
        let dst = safe_join(store_dir, rel)?;
        let bytes = std::fs::read(&src)
            .map_err(|e| FreezeError::Io(format!("snapshot {rel}: read: {e}")))?;
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| FreezeError::Io(format!("snapshot {rel}: mkdir: {e}")))?;
        }
        std::fs::write(&dst, &bytes)
            .map_err(|e| FreezeError::Io(format!("snapshot {rel}: write: {e}")))?;
        m.files.insert(rel.clone(), sha256_bytes(&bytes));
    }
    Ok(m)
}

/// Rewrites every frozen file in the working tree from the store, recreating
/// files the agent deleted. Returns the paths it changed.
///
/// The working tree is examined **before** the store is read, which is what
/// makes the common case — an eval where no test was touched — cost one read
/// per frozen file instead of two. The golden copy is opened, and validated
/// against the manifest hash, only for a file that has to be rewritten.
pub fn restore(
    repo_root: &Path,
    store_dir: &Path,
    m: &Manifest,
) -> Result<Vec<String>, FreezeError> {
    let mut changed = Vec::new();
    for (rel, want_hash) in &m.files {
        // Before any read or write: reads follow links just as writes do, so
        // this has to come first to avoid reading through one and concluding
        // the file is fine.
        no_symlink(repo_root, rel, "restore")?;
        let dst = safe_join(repo_root, rel)?;
        if let Ok(got) = std::fs::read(&dst) {
            if &sha256_bytes(&got) == want_hash {
                continue; // already the frozen content; the store need not be read
            }
        }

        no_symlink(store_dir, rel, "restore (store)")?;
        let src = safe_join(store_dir, rel)?;
        let bytes = std::fs::read(&src)
            .map_err(|e| FreezeError::Io(format!("restore {rel}: read store: {e}")))?;
        let got = sha256_bytes(&bytes);
        if &got != want_hash {
            return Err(FreezeError::StoreTampered(format!(
                "restore {rel}: store copy hashes to {got}, manifest records {want_hash}"
            )));
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| FreezeError::Io(format!("restore {rel}: mkdir: {e}")))?;
        }
        std::fs::write(&dst, &bytes)
            .map_err(|e| FreezeError::Io(format!("restore {rel}: write: {e}")))?;
        changed.push(rel.clone());
    }
    Ok(changed)
}

/// Reports which frozen files currently differ from baseline. A deleted file,
/// and a path with a symlink anywhere along it, both count as changed.
pub fn verify(repo_root: &Path, m: &Manifest) -> Result<Vec<String>, FreezeError> {
    let mut changed = Vec::new();
    for (rel, want_hash) in &m.files {
        if symlink_component(repo_root, rel)?.is_some() {
            // At least as suspicious as a deleted file — report it rather than
            // following the link to read whatever it points at.
            changed.push(rel.clone());
            continue;
        }
        let path = safe_join(repo_root, rel)?;
        match std::fs::read(&path) {
            Ok(bytes) => {
                if &sha256_bytes(&bytes) != want_hash {
                    changed.push(rel.clone());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => changed.push(rel.clone()),
            Err(e) => return Err(FreezeError::Io(format!("verify {rel}: {e}"))),
        }
    }
    Ok(changed)
}

/// Hashes the test material inside one source file: every item gated by a
/// `#[cfg(test)]`-mentioning attribute (a whole module, most commonly, but
/// also a bare `#[cfg(test)] fn` and the like) and every doc comment.
///
/// These cannot be frozen as files, because they live inside the very files
/// the agent is required to edit — restoring the whole file would erase the
/// optimization along with the test edit. So they are hashed at baseline and
/// any change is refused. See the design spec §6.
///
/// The `cfg_test` hash is taken over each qualifying item's **token stream**,
/// not its bytes, so reformatting or re-indenting a test module does not trip
/// the gate while changing an assertion does; the per-item digests are sorted
/// before being combined, so two independent test items changing places in
/// the file — not their content, just their order — does not trip it either.
/// Doc comments are hashed as text, because a doctest's content *is* its
/// text.
pub fn inline_hashes(source: &str) -> Result<InlineHashes, String> {
    inline_hashes_in(source, None).map_err(|e| e.to_string())
}

/// Same as [`inline_hashes`], but resolves a `#[cfg(test)] mod x;` (or any
/// further module declared without a body once inside one) against the
/// filesystem, relative to `repo_root` — `rel` is the repo-relative path of
/// the source file itself, matching a [`Manifest::inline`] key.
///
/// This is the real entry point: every caller that has a repository to read
/// from (`baseline` and `verify_inline`) must use this, not [`inline_hashes`],
/// or an out-of-line test module such as
///
/// ```text
/// // src/lib.rs
/// #[cfg(test)]
/// mod tests;      // Item::Mod with content == None
/// // src/tests.rs — the assertions themselves
/// ```
///
/// hashes to the same empty-string digest whether the assertions inside
/// `src/tests.rs` are real or gutted, because nothing in `src/lib.rs`'s own
/// token text changes either way.
pub fn inline_hashes_at(repo_root: &Path, rel: &str) -> Result<InlineHashes, FreezeError> {
    no_symlink(repo_root, rel, "hash inline tests")?;
    let path = safe_join(repo_root, rel)?;
    let text =
        std::fs::read_to_string(&path).map_err(|e| FreezeError::Io(format!("read {rel}: {e}")))?;
    inline_hashes_in(&text, Some((repo_root, rel)))
}

fn inline_hashes_in(source: &str, fs: Option<(&Path, &str)>) -> Result<InlineHashes, FreezeError> {
    let file =
        syn::parse_file(source).map_err(|e| FreezeError::Io(format!("parse source: {e}")))?;

    let mut cfg_test_digests = Vec::new();
    let mut guard = ModuleGuard::default();
    let content_sha256 = match fs {
        Some((repo_root, rel)) => {
            // The top-level file is the root of this traversal's declaration
            // path, so it counts as an ancestor of everything it (directly
            // or transitively) declares — a `mod x;` chain that loops back
            // to it is exactly as cyclic as one that loops back to any other
            // module on the path.
            guard.stack.insert(rel.to_string());
            guard
                .content
                .insert(rel.to_string(), sha256_bytes(source.as_bytes()));
            let loc = ModuleLoc::root(rel);
            collect_cfg_test(
                Some((repo_root, &loc)),
                &file.items,
                &mut cfg_test_digests,
                &mut guard,
                0,
            )?;
            Some(std::mem::take(&mut guard.content))
        }
        None => {
            collect_cfg_test(None, &file.items, &mut cfg_test_digests, &mut guard, 0)?;
            None
        }
    };
    cfg_test_digests.sort();

    let mut docs = String::new();
    collect_docs(&file.attrs, &mut docs);
    collect_item_docs(&file.items, &mut docs);

    Ok(InlineHashes {
        cfg_test: sha256_bytes(cfg_test_digests.concat().as_bytes()),
        doc: sha256_bytes(docs.as_bytes()),
        content_sha256,
    })
}

/// The directory containing a repo-relative, forward-slash path, `""` for
/// one with no `/` in it (the repository root).
fn dir_of(rel: &str) -> String {
    match rel.rfind('/') {
        Some(i) => rel[..i].to_string(),
        None => String::new(),
    }
}

/// The final path segment of a repo-relative, forward-slash path.
fn basename(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// `name` appended onto `base`, treating `""` as the repository root so the
/// join never produces a leading slash.
fn join_nonempty(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_string()
    } else {
        format!("{base}/{name}")
    }
}

/// Joins a `#[path = "..."]` attribute's value onto `base` (both
/// repo-relative, forward-slash, `""` meaning the repository root),
/// resolving any `.`/`..` components lexically and refusing to climb above
/// the root.
///
/// The value comes from source the agent controls, so it is untrusted input
/// in exactly the sense [`safe_join`]'s doc comment describes — this is that
/// same defense, applied before a path ever reaches `safe_join`.
fn join_rel(base: &str, part: &str) -> Result<String, FreezeError> {
    if part.contains('\\') || part.contains(':') {
        return Err(FreezeError::Io(format!(
            "#[path] value {part:?} must use forward slashes and no drive prefix"
        )));
    }
    if Path::new(part).is_absolute() {
        return Err(FreezeError::Io(format!(
            "#[path] value {part:?} must be relative"
        )));
    }
    let mut stack: Vec<&str> = if base.is_empty() {
        Vec::new()
    } else {
        base.split('/').collect()
    };
    for comp in part.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if stack.pop().is_none() {
                    return Err(FreezeError::Io(format!(
                        "#[path] value {part:?} escapes the repository root"
                    )));
                }
            }
            seg => stack.push(seg),
        }
    }
    Ok(stack.join("/"))
}

/// Where a file's module declarations resolve, mirroring rustc's own module
/// file conventions closely enough for the harness's purposes: the crate
/// root and any file literally named `mod.rs` contribute children directly
/// into their own directory; every other file's children live in a
/// subdirectory named after the module.
///
/// This is deliberately a heuristic based on the file's own name and path,
/// not on tracing how it was actually reached from the crate root (this
/// module hashes every in-scope source file independently — see
/// [`crate::discover::in_scope_sources`] — so the real declaration chain is
/// not available). It matches the standard convention every real crate that
/// is not deliberately obscure follows.
struct ModuleLoc {
    /// Directory a `#[path = "..."]` attribute is resolved against: the
    /// actual file's own directory, never the module-name subdirectory.
    file_dir: String,
    /// Directory an ordinary (no `#[path]`) child's `<name>.rs` or
    /// `<name>/mod.rs` is looked up in.
    children_dir: String,
}

impl ModuleLoc {
    /// The location for the in-scope source file itself: every crate root
    /// (`lib.rs`, `main.rs`) and every file scanned on its own is treated as
    /// contributing children into its own directory, matching how rustc
    /// treats the crate root — the only case this module needs to get right
    /// on its own, since a file reached via an ordinary `mod name;`
    /// (non-root, non-`mod.rs`) is instead given its location by
    /// [`ModuleLoc::for_file`] when it is resolved.
    fn root(rel: &str) -> ModuleLoc {
        let dir = dir_of(rel);
        ModuleLoc {
            file_dir: dir.clone(),
            children_dir: dir,
        }
    }

    /// The location for a file just resolved as the target of `mod name;`
    /// (with or without `#[path]`), given the repo-relative path it was
    /// found at and the identifier it was declared under.
    fn for_file(resolved_rel: &str, name: &str) -> ModuleLoc {
        let file_dir = dir_of(resolved_rel);
        let is_dir_owner = basename(resolved_rel) == "mod.rs";
        let children_dir = if is_dir_owner {
            file_dir.clone()
        } else {
            join_nonempty(&file_dir, name)
        };
        ModuleLoc {
            file_dir,
            children_dir,
        }
    }

    /// The location for an INLINE module (`mod x { ... }`, same file), which
    /// can itself carry `#[path]` to redirect where *its* children look.
    fn for_inline(&self, name: &str, path_attr: Option<&str>) -> Result<ModuleLoc, FreezeError> {
        let children_dir = match path_attr {
            Some(p) => join_rel(&self.file_dir, p)?,
            None => join_nonempty(&self.children_dir, name),
        };
        Ok(ModuleLoc {
            file_dir: self.file_dir.clone(),
            children_dir,
        })
    }
}

/// The maximum module-declaration nesting this will chase before refusing to
/// go further. Generous for any real crate; exists so a cyclic or
/// pathological chain of declarations fails loudly with a bounded amount of
/// work instead of recursing forever.
const MAX_MOD_DEPTH: usize = 64;

/// Resolves a `mod name;` declaration (no inline body) to the repo-relative
/// path of the file it names, honouring `#[path = "..."]` when present, and
/// refusing a symlink anywhere along the way exactly as every other frozen
/// path does.
///
/// Fails closed: a module the harness cannot resolve is exactly as dangerous
/// as one whose content changed, so "the file is missing" or "the path
/// attribute cannot be understood" is an error here, never treated as "no
/// tests to hash" — see the module doc comment on why absence must never
/// look identical to presence.
fn resolve_mod_file(
    repo_root: &Path,
    loc: &ModuleLoc,
    name: &str,
    path_attr: Option<&str>,
) -> Result<String, FreezeError> {
    let candidates: Vec<String> = match path_attr {
        Some(p) => vec![join_rel(&loc.file_dir, p)?],
        None => {
            let dir = join_nonempty(&loc.children_dir, name);
            vec![
                join_nonempty(&loc.children_dir, &format!("{name}.rs")),
                format!("{dir}/mod.rs"),
            ]
        }
    };
    for cand in &candidates {
        no_symlink(repo_root, cand, "resolve module")?;
        let abs = safe_join(repo_root, cand)?;
        if std::fs::metadata(&abs)
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            return Ok(cand.clone());
        }
    }
    Err(FreezeError::Io(format!(
        "module {name:?} is declared without a body (`mod {name};`) but its file could not be \
         found — looked for {}",
        candidates.join(" or "),
    )))
}

/// Per-[`inline_hashes_in`] traversal state, threaded through every call
/// that can resolve a `mod x;` declaration against the real filesystem.
///
/// A module reached twice within the SAME top-level traversal is not
/// necessarily a cycle: an ordinary diamond — one test-support module
/// declared from two different, non-cyclic parents — reaches the same file
/// twice too, and is entirely legitimate Rust. What makes a cycle a cycle is
/// a module reappearing among its OWN ancestors (the declaration path
/// currently being resolved), not merely having been visited earlier and
/// already finished. `stack` tracks exactly that ancestor path; `memo` and
/// `content` are keyed independently and simply accumulate across the whole
/// traversal, diamond or not.
#[derive(Default)]
struct ModuleGuard {
    /// Repo-relative paths on the CURRENT declaration path, i.e. this
    /// module's ancestors. A path reappearing here — not merely present in
    /// `memo` — is a genuine cycle.
    stack: HashSet<String>,
    /// Every module file's already-fully-resolved hash, keyed by its own
    /// path together with the exact location (`file_dir`/`children_dir`) it
    /// was reached under. The location has to be part of the key, not just
    /// the path: the same physical file reached under two different
    /// declared names (via `#[path]`) resolves ITS OWN children
    /// differently — see [`ModuleLoc::for_file`] — so a hash computed under
    /// one location must never be reused for another. This is what makes a
    /// diamond cheap, not merely correct: the shared module is parsed once,
    /// not once per parent that declares it.
    memo: BTreeMap<(String, String, String), String>,
    /// Raw-byte SHA-256 of every file actually read during this traversal
    /// (the top-level file plus every out-of-line module file reached),
    /// keyed by repo-relative path. Folded into
    /// [`InlineHashes::content_sha256`] by [`inline_hashes_in`].
    content: BTreeMap<String, String>,
}

/// Hashes an out-of-line module file's entire token stream, plus — since
/// everything inside it is already within a test-gated island regardless of
/// its own attributes — every further module IT declares without a body,
/// recursively. A symlink, an unreadable or unparseable file, and unbounded
/// or cyclic nesting are all refused rather than silently skipped.
fn hash_module_file(
    repo_root: &Path,
    rel: &str,
    loc: &ModuleLoc,
    depth: usize,
    guard: &mut ModuleGuard,
) -> Result<String, FreezeError> {
    let key = (
        rel.to_string(),
        loc.file_dir.clone(),
        loc.children_dir.clone(),
    );
    if let Some(hash) = guard.memo.get(&key) {
        // Already fully resolved via another, unrelated parent earlier in
        // this same traversal — a diamond, not a cycle. Reuse it rather
        // than reading and re-parsing the file a second time.
        return Ok(hash.clone());
    }
    if depth > MAX_MOD_DEPTH {
        return Err(FreezeError::Io(format!(
            "module nesting is more than {MAX_MOD_DEPTH} deep resolving {rel:?} — refusing to \
             recurse further"
        )));
    }
    if !guard.stack.insert(rel.to_string()) {
        return Err(FreezeError::Io(format!(
            "cyclic module declaration involving {rel:?}"
        )));
    }
    no_symlink(repo_root, rel, "hash module")?;
    let path = safe_join(repo_root, rel)?;
    let text =
        std::fs::read_to_string(&path).map_err(|e| FreezeError::Io(format!("read {rel}: {e}")))?;
    let file = syn::parse_file(&text).map_err(|e| FreezeError::Io(format!("parse {rel}: {e}")))?;
    guard
        .content
        .insert(rel.to_string(), sha256_bytes(text.as_bytes()));

    let mut parts = vec![file.to_token_stream().to_string()];
    let result = collect_declared_mods(&file.items, repo_root, loc, depth, guard, &mut parts);
    // Popped whether this succeeded or not: on success it is no longer an
    // ancestor of anything; on failure the whole traversal is about to
    // abort anyway, so a stale stack entry cannot mislead anyone.
    guard.stack.remove(rel);
    result?;

    let hash = sha256_bytes(parts.concat().as_bytes());
    guard.memo.insert(key, hash.clone());
    Ok(hash)
}

/// Inside an already test-gated island: walks every item looking for a
/// further module declared without a body — its own `#[cfg(test)]` no
/// longer matters, since it inherits the gate from its parent — and appends
/// each one's recursive hash. Also descends into INLINE modules to find such
/// declarations nested inside them.
fn collect_declared_mods(
    items: &[syn::Item],
    repo_root: &Path,
    loc: &ModuleLoc,
    depth: usize,
    guard: &mut ModuleGuard,
    parts: &mut Vec<String>,
) -> Result<(), FreezeError> {
    for item in items {
        let syn::Item::Mod(m) = item else { continue };
        match &m.content {
            Some((_, inner)) => {
                let path_attr = extract_path_attr(&m.attrs);
                let child_loc = loc.for_inline(&m.ident.to_string(), path_attr.as_deref())?;
                collect_declared_mods(inner, repo_root, &child_loc, depth, guard, parts)?;
            }
            None => {
                let name = m.ident.to_string();
                let path_attr = extract_path_attr(&m.attrs);
                let child_rel = resolve_mod_file(repo_root, loc, &name, path_attr.as_deref())?;
                let child_loc = ModuleLoc::for_file(&child_rel, &name);
                parts.push(hash_module_file(
                    repo_root,
                    &child_rel,
                    &child_loc,
                    depth + 1,
                    guard,
                )?);
            }
        }
    }
    Ok(())
}

/// The string value of a `#[path = "..."]` attribute, if present.
fn extract_path_attr(attrs: &[syn::Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("path") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &attr.meta {
            if let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
            {
                return Some(s.value());
            }
        }
    }
    None
}

/// Whether an attribute is `#[cfg(...)]` with `test` mentioned in a position
/// that gates *on* being a test build — a bare `#[cfg(test)]`, or `test`
/// nested inside `all(...)`/`any(...)` at any depth.
///
/// This walks the `cfg` predicate structurally rather than substring-matching
/// its tokens, which matters because `#[cfg(not(test))]` — the attribute for
/// marking something as *production-only* — contains the literal text `test`
/// too. A substring check would misclassify it as test material and refuse
/// any ordinary edit to the code it guards; walking the tree instead lets
/// `not(...)` suppress whatever is nested inside it, so only a `test` that
/// actually has to hold for the item to compile counts.
fn is_cfg_test(attr: &syn::Attribute) -> bool {
    if !attr.path().is_ident("cfg") {
        return false;
    }
    match attr.parse_args::<syn::Meta>() {
        Ok(meta) => meta_mentions_test(&meta),
        // An attribute named `cfg` whose contents don't even parse as a
        // predicate cannot be `cfg(test)`; the build gate will report the
        // syntax error properly a moment later.
        Err(_) => false,
    }
}

/// Whether `meta` — one `cfg(...)` predicate — mentions `test` outside the
/// scope of a `not(...)`.
fn meta_mentions_test(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(p) => p.is_ident("test"),
        syn::Meta::List(list) => {
            if list.path.is_ident("not") {
                // Whatever is nested inside `not(...)` is exactly what must
                // NOT hold, so a `test` in there marks this as
                // production-only, not test-gated.
                return false;
            }
            if list.path.is_ident("all") || list.path.is_ident("any") {
                if let Ok(nested) = list.parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                ) {
                    return nested.iter().any(meta_mentions_test);
                }
            }
            false
        }
        _ => false,
    }
}

/// The attributes on any item kind that can carry `#[cfg(test)]`.
///
/// `syn::Item` has no attrs accessor shared across variants, so this matches
/// explicitly; an item kind not listed here (only `Verbatim`, tokens `syn`
/// itself could not interpret) carries no attributes this function can see,
/// which is the fail-safe direction: such an item is never mistaken for test
/// material, but it also can never be *exempted* from a hash by omission.
fn item_attrs(item: &syn::Item) -> &[syn::Attribute] {
    match item {
        syn::Item::Const(i) => &i.attrs,
        syn::Item::Enum(i) => &i.attrs,
        syn::Item::ExternCrate(i) => &i.attrs,
        syn::Item::Fn(i) => &i.attrs,
        syn::Item::ForeignMod(i) => &i.attrs,
        syn::Item::Impl(i) => &i.attrs,
        syn::Item::Macro(i) => &i.attrs,
        syn::Item::Mod(i) => &i.attrs,
        syn::Item::Static(i) => &i.attrs,
        syn::Item::Struct(i) => &i.attrs,
        syn::Item::Trait(i) => &i.attrs,
        syn::Item::TraitAlias(i) => &i.attrs,
        syn::Item::Type(i) => &i.attrs,
        syn::Item::Union(i) => &i.attrs,
        syn::Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

/// Collects one digest per item gated by a test-mentioning `cfg`, at any
/// nesting depth, recursing into ordinary (non-test-gated) modules so a test
/// item nested inside one is still covered.
///
/// Hashing per item rather than concatenating raw token text, and letting the
/// caller sort the result, is what makes two sibling test items changing
/// places in the file a no-op: nothing here depends on which one came first.
///
/// `fs` is `Some((repo_root, loc))` when there is a real file to resolve a
/// `mod x;` declared without a body against — see [`inline_hashes_at`] — and
/// `None` for the pure-string [`inline_hashes`], which cannot resolve one and
/// fails closed instead of silently treating it as having no tests.
fn collect_cfg_test(
    fs: Option<(&Path, &ModuleLoc)>,
    items: &[syn::Item],
    out: &mut Vec<String>,
    guard: &mut ModuleGuard,
    depth: usize,
) -> Result<(), FreezeError> {
    for item in items {
        if item_attrs(item).iter().any(is_cfg_test) {
            if let syn::Item::Mod(m) = item {
                if m.content.is_none() {
                    let Some((repo_root, loc)) = fs else {
                        return Err(FreezeError::Io(format!(
                            "module {:?} is declared without a body (`mod {};`) and cannot be \
                             resolved without a file path — this source must be hashed via \
                             inline_hashes_at, not inline_hashes",
                            m.ident, m.ident
                        )));
                    };
                    let name = m.ident.to_string();
                    let path_attr = extract_path_attr(&m.attrs);
                    let child_rel = resolve_mod_file(repo_root, loc, &name, path_attr.as_deref())?;
                    let child_loc = ModuleLoc::for_file(&child_rel, &name);
                    out.push(hash_module_file(
                        repo_root,
                        &child_rel,
                        &child_loc,
                        depth + 1,
                        guard,
                    )?);
                    continue;
                }
            }
            out.push(sha256_bytes(item.to_token_stream().to_string().as_bytes()));
            continue;
        }
        if let syn::Item::Mod(m) = item {
            if let Some((_, inner)) = &m.content {
                let child_fs = match fs {
                    Some((repo_root, loc)) => {
                        let path_attr = extract_path_attr(&m.attrs);
                        let child_loc =
                            loc.for_inline(&m.ident.to_string(), path_attr.as_deref())?;
                        Some((repo_root, child_loc))
                    }
                    None => None,
                };
                collect_cfg_test(
                    child_fs.as_ref().map(|(r, l)| (*r, l)),
                    inner,
                    out,
                    guard,
                    depth + 1,
                )?;
            }
        }
    }
    Ok(())
}

/// Appends every doc-comment line's text.
///
/// A `#[doc = include_str!("...")]` attribute is the one case whose actual
/// doctest content lives in a file this cannot see (resolving it would need
/// the containing file's own path, which most callers of this function do
/// not have): at minimum, the macro's own token text — which contains the
/// included path — is hashed, so a changed path is still detected even
/// though the included file's content is not.
fn collect_docs(attrs: &[syn::Attribute], out: &mut String) {
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        let syn::Meta::NameValue(nv) = &attr.meta else {
            continue;
        };
        match &nv.value {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) => {
                out.push_str(&s.value());
                out.push('\n');
            }
            syn::Expr::Macro(syn::ExprMacro { mac, .. }) => {
                out.push_str(&mac.to_token_stream().to_string());
                out.push('\n');
            }
            _ => {}
        }
    }
}

/// Every doc comment on the fields of a struct, tuple struct, enum variant,
/// or union.
fn collect_field_docs(fields: &syn::Fields, out: &mut String) {
    match fields {
        syn::Fields::Named(f) => {
            for field in &f.named {
                collect_docs(&field.attrs, out);
            }
        }
        syn::Fields::Unnamed(f) => {
            for field in &f.unnamed {
                collect_docs(&field.attrs, out);
            }
        }
        syn::Fields::Unit => {}
    }
}

/// Walks every item that can carry a doc comment, recursing into modules,
/// impl blocks, traits, `extern` blocks, and the fields of a struct, enum or
/// union.
fn collect_item_docs(items: &[syn::Item], out: &mut String) {
    for item in items {
        match item {
            syn::Item::Fn(i) => collect_docs(&i.attrs, out),
            syn::Item::Struct(i) => {
                collect_docs(&i.attrs, out);
                collect_field_docs(&i.fields, out);
            }
            syn::Item::Enum(i) => {
                collect_docs(&i.attrs, out);
                for v in &i.variants {
                    collect_docs(&v.attrs, out);
                    collect_field_docs(&v.fields, out);
                }
            }
            syn::Item::Union(i) => {
                collect_docs(&i.attrs, out);
                for f in &i.fields.named {
                    collect_docs(&f.attrs, out);
                }
            }
            syn::Item::Trait(i) => {
                collect_docs(&i.attrs, out);
                for it in &i.items {
                    match it {
                        syn::TraitItem::Fn(f) => collect_docs(&f.attrs, out),
                        syn::TraitItem::Const(c) => collect_docs(&c.attrs, out),
                        syn::TraitItem::Type(t) => collect_docs(&t.attrs, out),
                        _ => {}
                    }
                }
            }
            syn::Item::Const(i) => collect_docs(&i.attrs, out),
            syn::Item::Static(i) => collect_docs(&i.attrs, out),
            syn::Item::Type(i) => collect_docs(&i.attrs, out),
            syn::Item::Macro(i) => collect_docs(&i.attrs, out),
            syn::Item::ForeignMod(i) => {
                collect_docs(&i.attrs, out);
                for it in &i.items {
                    match it {
                        syn::ForeignItem::Fn(f) => collect_docs(&f.attrs, out),
                        syn::ForeignItem::Static(s) => collect_docs(&s.attrs, out),
                        syn::ForeignItem::Type(t) => collect_docs(&t.attrs, out),
                        syn::ForeignItem::Macro(m) => collect_docs(&m.attrs, out),
                        _ => {}
                    }
                }
            }
            syn::Item::Impl(i) => {
                collect_docs(&i.attrs, out);
                for it in &i.items {
                    match it {
                        syn::ImplItem::Fn(f) => collect_docs(&f.attrs, out),
                        syn::ImplItem::Const(c) => collect_docs(&c.attrs, out),
                        syn::ImplItem::Type(t) => collect_docs(&t.attrs, out),
                        _ => {}
                    }
                }
            }
            syn::Item::Mod(m) => {
                collect_docs(&m.attrs, out);
                if let Some((_, inner)) = &m.content {
                    collect_item_docs(inner, out);
                }
            }
            _ => {}
        }
    }
}

/// Whether every file `want.content_sha256` records is still byte-for-byte
/// identical to baseline, which makes re-parsing to recompute
/// `want.cfg_test`/`want.doc` redundant: that computation is a pure function
/// of exactly this content (see [`inline_hashes_in`]), so unchanged bytes
/// everywhere in the set guarantee an unchanged result.
///
/// Fails closed on anything ambiguous, treating it as "cannot skip" rather
/// than "unchanged" — a missing field (`None`, from a manifest written
/// before it existed, or from a hash with no filesystem behind it), a
/// missing or unreadable file, or a symlink anywhere along a recorded path.
/// A symlinked path is rejected outright, exactly as [`verify`] treats one
/// for frozen files: at least as suspicious as a deleted file, never
/// something to read through and compare by content, since identical bytes
/// reached via a link do not mean the same thing as identical bytes reached
/// directly (see [`symlink_component`]'s doc comment). This function is
/// purely a speed short-circuit — returning `false` here never causes a
/// missed change, only a slower, fully-correct re-parse via
/// [`inline_hashes_at`], which is exactly the pre-existing check.
fn content_unchanged(repo_root: &Path, want: &InlineHashes) -> bool {
    let Some(content) = &want.content_sha256 else {
        return false;
    };
    for (rel, want_hash) in content {
        match symlink_component(repo_root, rel) {
            Ok(None) => {}
            Ok(Some(_)) | Err(_) => return false,
        }
        let Ok(path) = safe_join(repo_root, rel) else {
            return false;
        };
        match std::fs::read(&path) {
            Ok(bytes) if &sha256_bytes(&bytes) == want_hash => {}
            _ => return false,
        }
    }
    true
}

/// Reports which in-scope source files' inline tests differ from baseline.
///
/// A deleted or unreadable file counts as changed: its tests are certainly not
/// what they were.
///
/// Gate 5's dominant cost used to be a full `syn::parse_file` over every
/// in-scope file on every eval, whether or not it changed. Most evals change
/// none of them, so [`content_unchanged`] first checks raw bytes — a cost
/// linear in file size but with no parsing at all — and only falls through
/// to the real parse-based check when that cannot prove nothing changed.
/// This changes nothing about what counts as "changed": the raw-byte check
/// is content-only (it never consults git, so a gitignored in-scope file is
/// exactly as visible to it as to the real check — see
/// [`crate::discover::in_scope_sources`]), and comparing only `cfg_test`
/// and `doc` below — not `content_sha256` — on the slow path is what the
/// gate always compared, before this field existed.
pub fn verify_inline(repo_root: &Path, m: &Manifest) -> Result<Vec<String>, FreezeError> {
    let mut changed = Vec::new();
    for (rel, want) in &m.inline {
        if content_unchanged(repo_root, want) {
            continue;
        }
        match inline_hashes_at(repo_root, rel) {
            Ok(got) if got.cfg_test == want.cfg_test && got.doc == want.doc => {}
            // Anything else — a deleted or symlinked file, one that no
            // longer parses (an unparseable file is a syntax error the build
            // gate, which runs after this one, will report properly; here it
            // simply cannot be hashed), or a `mod x;` declaration whose file
            // can no longer be resolved — counts as changed. Absence must
            // never look identical to presence.
            _ => changed.push(rel.clone()),
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: std::path::PathBuf,
        store: std::path::PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let store = dir.path().join("store");
        fs::create_dir_all(repo.join("tests")).unwrap();
        fs::create_dir_all(&store).unwrap();
        fs::write(repo.join("tests/it.rs"), b"assert_eq!(1, 1);\n").unwrap();
        Fixture {
            _dir: dir,
            repo,
            store,
        }
    }

    fn files() -> Vec<String> {
        vec!["tests/it.rs".to_string()]
    }

    #[test]
    fn snapshot_then_restore_round_trips() {
        let f = fixture();
        let m = snapshot(&f.repo, &f.store, &files()).unwrap();
        assert_eq!(m.files.len(), 1);
        fs::write(f.repo.join("tests/it.rs"), b"assert!(true); // weakened\n").unwrap();
        let changed = restore(&f.repo, &f.store, &m).unwrap();
        assert_eq!(changed, vec!["tests/it.rs"]);
        assert_eq!(
            fs::read(f.repo.join("tests/it.rs")).unwrap(),
            b"assert_eq!(1, 1);\n"
        );
    }

    #[test]
    fn restore_recreates_a_deleted_file() {
        let f = fixture();
        let m = snapshot(&f.repo, &f.store, &files()).unwrap();
        fs::remove_file(f.repo.join("tests/it.rs")).unwrap();
        let changed = restore(&f.repo, &f.store, &m).unwrap();
        assert_eq!(changed, vec!["tests/it.rs"]);
        assert!(f.repo.join("tests/it.rs").exists());
    }

    // The common case — no test was touched — must not read the golden copy
    // at all, so restore reports nothing changed.
    #[test]
    fn restore_is_a_no_op_when_nothing_was_touched() {
        let f = fixture();
        let m = snapshot(&f.repo, &f.store, &files()).unwrap();
        assert!(restore(&f.repo, &f.store, &m).unwrap().is_empty());
    }

    #[test]
    fn verify_reports_edited_and_deleted_files() {
        let f = fixture();
        let m = snapshot(&f.repo, &f.store, &files()).unwrap();
        assert!(verify(&f.repo, &m).unwrap().is_empty());
        fs::write(f.repo.join("tests/it.rs"), b"different\n").unwrap();
        assert_eq!(verify(&f.repo, &m).unwrap(), vec!["tests/it.rs"]);
        fs::remove_file(f.repo.join("tests/it.rs")).unwrap();
        assert_eq!(verify(&f.repo, &m).unwrap(), vec!["tests/it.rs"]);
    }

    // A rewritten golden copy would be restored into the working tree by every
    // later eval — the exact outcome freezing exists to prevent — so the store
    // is checked against the manifest hash, not trusted.
    #[test]
    fn a_tampered_store_copy_is_detected() {
        let f = fixture();
        let m = snapshot(&f.repo, &f.store, &files()).unwrap();
        fs::write(f.store.join("tests/it.rs"), b"assert!(true); // weakened\n").unwrap();
        fs::write(f.repo.join("tests/it.rs"), b"anything else\n").unwrap();
        match restore(&f.repo, &f.store, &m) {
            Err(FreezeError::StoreTampered(msg)) => assert!(msg.contains("tests/it.rs"), "{msg}"),
            other => panic!("expected StoreTampered, got {other:?}"),
        }
    }

    #[test]
    fn a_manifest_path_escaping_the_root_is_refused() {
        let f = fixture();
        let mut m = Manifest::default();
        m.files.insert("../escape.rs".into(), "0".repeat(64));
        assert!(restore(&f.repo, &f.store, &m).is_err());
    }

    #[test]
    fn an_absolute_manifest_path_is_refused() {
        let f = fixture();
        let mut m = Manifest::default();
        let abs = if cfg!(windows) {
            "C:\\evil.rs"
        } else {
            "/etc/evil.rs"
        };
        m.files.insert(abs.into(), "0".repeat(64));
        assert!(restore(&f.repo, &f.store, &m).is_err());
    }

    // Rust's Unix `Path` parser never treats `\` as a separator, so a
    // backslash-separated escape or drive prefix is neither absolute nor has
    // a `..` component there — it must be refused structurally instead, on
    // every platform, so a manifest written on Windows and read on Unix (or
    // vice versa) is refused identically.
    #[test]
    fn backslash_and_drive_paths_are_refused_on_every_platform() {
        let f = fixture();
        for bad in ["C:\\evil.rs", "..\\escape.rs", "a\\..\\..\\escape.rs"] {
            let mut m = Manifest::default();
            m.files.insert(bad.into(), "0".repeat(64));
            assert!(
                restore(&f.repo, &f.store, &m).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_test_file_is_refused_by_both_snapshot_and_restore() {
        let f = fixture();
        let outside = f.repo.parent().unwrap().join("outside.rs");
        fs::write(&outside, b"outside\n").unwrap();
        fs::remove_file(f.repo.join("tests/it.rs")).unwrap();
        std::os::unix::fs::symlink(&outside, f.repo.join("tests/it.rs")).unwrap();
        assert!(matches!(
            snapshot(&f.repo, &f.store, &files()),
            Err(FreezeError::Symlink(_))
        ));

        // And on the restore side, where writing through the link would put
        // frozen content outside the repository entirely.
        let f2 = fixture();
        let m = snapshot(&f2.repo, &f2.store, &files()).unwrap();
        fs::remove_file(f2.repo.join("tests/it.rs")).unwrap();
        std::os::unix::fs::symlink(&outside, f2.repo.join("tests/it.rs")).unwrap();
        assert!(matches!(
            restore(&f2.repo, &f2.store, &m),
            Err(FreezeError::Symlink(_))
        ));
    }

    // Checking only the final component is not enough: read and write resolve
    // the whole path, so swapping a PARENT DIRECTORY for a link redirects the
    // write just as effectively, and an lstat on the file then reports a
    // perfectly ordinary regular file.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_parent_directory_is_refused() {
        let f = fixture();
        let m = snapshot(&f.repo, &f.store, &files()).unwrap();
        let elsewhere = f.repo.parent().unwrap().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("it.rs"), b"decoy\n").unwrap();
        fs::remove_dir_all(f.repo.join("tests")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, f.repo.join("tests")).unwrap();
        assert!(matches!(
            restore(&f.repo, &f.store, &m),
            Err(FreezeError::Symlink(_))
        ));
    }

    // The deliberate exception `symlink_component` documents: `root` itself
    // is never examined, only components beneath it, so a repository
    // legitimately reached through a symlinked ancestor (macOS's `/tmp`, a
    // home directory on a linked volume) is not tampering. No existing test
    // exercised this directly before now — every other fixture here builds
    // its repo under a plain `tempfile::tempdir()` path, and the ordinary
    // symlink tests above only ever place the link BENEATH the repo root.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_ancestor_of_root_itself_is_not_tampering() {
        let outer = tempfile::tempdir().unwrap();
        let real = outer.path().join("real");
        fs::create_dir_all(real.join("tests")).unwrap();
        fs::write(real.join("tests/it.rs"), b"assert_eq!(1, 1);\n").unwrap();
        let link = outer.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // `repo_root` is reached only through `link`, deliberately not
        // canonicalized away.
        let repo_root = link.clone();
        let store = outer.path().join("store");
        fs::create_dir_all(&store).unwrap();

        assert_eq!(symlink_component(&repo_root, "tests/it.rs").unwrap(), None);
        let m = snapshot(&repo_root, &store, &files()).unwrap();
        assert!(restore(&repo_root, &store, &m).unwrap().is_empty());
        assert!(verify(&repo_root, &m).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn verify_reports_a_symlinked_path_as_changed_rather_than_following_it() {
        let f = fixture();
        let m = snapshot(&f.repo, &f.store, &files()).unwrap();
        let outside = f.repo.parent().unwrap().join("outside2.rs");
        fs::write(&outside, b"assert_eq!(1, 1);\n").unwrap(); // identical bytes!
        fs::remove_file(f.repo.join("tests/it.rs")).unwrap();
        std::os::unix::fs::symlink(&outside, f.repo.join("tests/it.rs")).unwrap();
        assert_eq!(verify(&f.repo, &m).unwrap(), vec!["tests/it.rs"]);
    }

    #[test]
    fn manifest_saves_and_loads() {
        let f = fixture();
        let m = snapshot(&f.repo, &f.store, &files()).unwrap();
        let path = f.store.join("manifest.json");
        m.save(&path).unwrap();
        assert_eq!(Manifest::load(&path).unwrap(), m);
    }

    const WITH_INLINE: &str = r#"
/// Adds two numbers.
///
/// ```
/// assert_eq!(demo::add(1, 2), 3);
/// ```
pub fn add(a: i32, b: i32) -> i32 { a + b }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adds() { assert_eq!(add(1, 2), 3); }
}
"#;

    #[test]
    fn hashing_is_stable_for_identical_source() {
        let a = inline_hashes(WITH_INLINE).unwrap();
        let b = inline_hashes(WITH_INLINE).unwrap();
        assert_eq!(a, b);
        assert!(!a.cfg_test.is_empty());
        assert!(!a.doc.is_empty());
    }

    // The agent is entitled to edit these files, so ordinary work on the
    // non-test code must not trip the gate.
    #[test]
    fn changing_the_implementation_does_not_change_the_hashes() {
        let optimized = WITH_INLINE.replace("{ a + b }", "{ a.wrapping_add(b) }");
        assert_eq!(
            inline_hashes(WITH_INLINE).unwrap(),
            inline_hashes(&optimized).unwrap()
        );
    }

    // Hashing over parsed tokens, not raw bytes, is what buys this.
    #[test]
    fn reformatting_the_test_module_does_not_change_the_cfg_test_hash() {
        let reformatted = WITH_INLINE.replace(
            "    fn adds() { assert_eq!(add(1, 2), 3); }",
            "    fn adds() {\n        assert_eq!(add(1, 2), 3);\n    }",
        );
        assert_eq!(
            inline_hashes(WITH_INLINE).unwrap().cfg_test,
            inline_hashes(&reformatted).unwrap().cfg_test
        );
    }

    #[test]
    fn weakening_an_assertion_changes_the_cfg_test_hash() {
        let weakened = WITH_INLINE.replace("assert_eq!(add(1, 2), 3);", "assert!(true);");
        assert_ne!(
            inline_hashes(WITH_INLINE).unwrap().cfg_test,
            inline_hashes(&weakened).unwrap().cfg_test
        );
    }

    #[test]
    fn deleting_the_test_module_changes_the_cfg_test_hash() {
        let start = WITH_INLINE.find("#[cfg(test)]").unwrap();
        let deleted = &WITH_INLINE[..start];
        assert_ne!(
            inline_hashes(WITH_INLINE).unwrap().cfg_test,
            inline_hashes(deleted).unwrap().cfg_test
        );
    }

    #[test]
    fn weakening_a_doctest_changes_the_doc_hash() {
        let weakened = WITH_INLINE.replace(
            "/// assert_eq!(demo::add(1, 2), 3);",
            "/// let _ = demo::add(1, 2);",
        );
        assert_ne!(
            inline_hashes(WITH_INLINE).unwrap().doc,
            inline_hashes(&weakened).unwrap().doc
        );
    }

    // A file that does not parse cannot be hashed, and guessing would be
    // worse than saying so: an unparseable file is reported, not skipped.
    #[test]
    fn unparseable_source_is_an_error() {
        assert!(inline_hashes("fn broken( {").is_err());
    }

    #[test]
    fn a_file_with_no_inline_tests_hashes_to_the_empty_marker() {
        let h = inline_hashes("pub fn f() {}\n").unwrap();
        let h2 = inline_hashes("pub fn g() -> u8 { 7 }\n").unwrap();
        assert_eq!(h, h2, "two files with no inline tests must agree");
    }

    #[test]
    fn verify_inline_finds_the_changed_file_and_only_that_file() {
        let f = fixture();
        fs::create_dir_all(f.repo.join("src")).unwrap();
        fs::write(f.repo.join("src/lib.rs"), WITH_INLINE).unwrap();
        fs::write(f.repo.join("src/other.rs"), "pub fn g() {}\n").unwrap();

        let mut m = Manifest::default();
        for rel in ["src/lib.rs", "src/other.rs"] {
            let text = fs::read_to_string(f.repo.join(rel)).unwrap();
            m.inline
                .insert(rel.to_string(), inline_hashes(&text).unwrap());
        }
        assert!(verify_inline(&f.repo, &m).unwrap().is_empty());

        let weakened = WITH_INLINE.replace("assert_eq!(add(1, 2), 3);", "assert!(true);");
        fs::write(f.repo.join("src/lib.rs"), weakened).unwrap();
        assert_eq!(verify_inline(&f.repo, &m).unwrap(), vec!["src/lib.rs"]);
    }

    #[test]
    fn verify_inline_reports_a_deleted_source_file() {
        let f = fixture();
        fs::create_dir_all(f.repo.join("src")).unwrap();
        fs::write(f.repo.join("src/lib.rs"), WITH_INLINE).unwrap();
        let mut m = Manifest::default();
        m.inline
            .insert("src/lib.rs".into(), inline_hashes(WITH_INLINE).unwrap());
        fs::remove_file(f.repo.join("src/lib.rs")).unwrap();
        assert_eq!(verify_inline(&f.repo, &m).unwrap(), vec!["src/lib.rs"]);
    }

    // Regression: a bare `#[cfg(test)]` item outside any `mod` block is just
    // as much test material as a `#[cfg(test)] mod tests { ... }`, and must
    // be caught the same way.
    #[test]
    fn a_bare_cfg_test_function_changing_changes_the_hash() {
        const SRC: &str = r#"
#[cfg(test)]
fn test_helper() -> i32 { 1 }
"#;
        let changed = SRC.replace("{ 1 }", "{ 999 }");
        assert_ne!(
            inline_hashes(SRC).unwrap().cfg_test,
            inline_hashes(&changed).unwrap().cfg_test
        );
    }

    #[test]
    fn a_bare_cfg_test_top_level_test_fn_changing_changes_the_hash() {
        const SRC: &str = r#"
#[cfg(test)]
#[test]
fn top_level_test() { assert_eq!(1 + 1, 2); }
"#;
        let weakened = SRC.replace("assert_eq!(1 + 1, 2);", "assert!(true);");
        assert_ne!(
            inline_hashes(SRC).unwrap().cfg_test,
            inline_hashes(&weakened).unwrap().cfg_test
        );
    }

    // Regression: `#[cfg(not(test))]` marks something as production-only. It
    // contains the literal text "test" too, so a substring check would
    // wrongly treat ordinary edits to it as changing test material.
    #[test]
    fn cfg_not_test_is_not_test_gated() {
        const SRC: &str = r#"
#[cfg(not(test))]
mod production_only {
    pub fn real_impl() -> i32 { 1 }
}
"#;
        let changed = SRC.replace("{ 1 }", "{ 2 }");
        assert_eq!(
            inline_hashes(SRC).unwrap().cfg_test,
            inline_hashes(&changed).unwrap().cfg_test
        );
    }

    // Regression: `test` nested inside `all(...)` or `any(...)` must still
    // count as test-gated — the conservative reading a gate needs.
    #[test]
    fn cfg_all_and_any_with_test_are_still_test_gated() {
        const ALL_SRC: &str = r#"
#[cfg(all(test, feature = "x"))]
fn helper() -> i32 { 1 }
"#;
        let all_changed = ALL_SRC.replace("{ 1 }", "{ 2 }");
        assert_ne!(
            inline_hashes(ALL_SRC).unwrap().cfg_test,
            inline_hashes(&all_changed).unwrap().cfg_test
        );

        const ANY_SRC: &str = r#"
#[cfg(any(test, foo))]
fn helper() -> i32 { 1 }
"#;
        let any_changed = ANY_SRC.replace("{ 1 }", "{ 2 }");
        assert_ne!(
            inline_hashes(ANY_SRC).unwrap().cfg_test,
            inline_hashes(&any_changed).unwrap().cfg_test
        );
    }

    // Regression: two independent `#[cfg(test)]` modules swapping position in
    // the file must not trip the gate — only their content should. The brief
    // promises moving a test module is a no-op; with more than one module in
    // the file, that promise only holds if the per-item digests are combined
    // order-independently.
    #[test]
    fn swapping_two_sibling_cfg_test_modules_does_not_change_the_hash() {
        const ORIGINAL: &str = r#"
#[cfg(test)]
mod tests_a {
    #[test]
    fn a() { assert_eq!(1, 1); }
}

#[cfg(test)]
mod tests_b {
    #[test]
    fn b() { assert_eq!(2, 2); }
}
"#;
        const SWAPPED: &str = r#"
#[cfg(test)]
mod tests_b {
    #[test]
    fn b() { assert_eq!(2, 2); }
}

#[cfg(test)]
mod tests_a {
    #[test]
    fn a() { assert_eq!(1, 1); }
}
"#;
        assert_eq!(
            inline_hashes(ORIGINAL).unwrap().cfg_test,
            inline_hashes(SWAPPED).unwrap().cfg_test
        );

        let content_changed = ORIGINAL.replace("assert_eq!(2, 2);", "assert!(true);");
        assert_ne!(
            inline_hashes(ORIGINAL).unwrap().cfg_test,
            inline_hashes(&content_changed).unwrap().cfg_test
        );
    }

    // C1: `#[cfg(test)] mod tests;` declared without a body — the idiomatic
    // large-crate layout, where the assertions live in their own file that
    // carries no `#[cfg(test)]` of its own. Before this fix, `inline_hashes`
    // only ever saw `src/lib.rs`'s own token text, which is the literal three
    // tokens `mod tests ;` whether the assertions in `src/tests.rs` are real
    // or gutted — so the digest could not tell the two apart.
    #[test]
    fn an_out_of_line_cfg_test_module_is_hashed_via_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\n#[cfg(test)]\nmod tests;\n",
        )
        .unwrap();
        fs::write(
            root.join("src/tests.rs"),
            "use super::*;\n#[test]\nfn adds() { assert_eq!(add(1, 2), 3); }\n",
        )
        .unwrap();

        let baseline = inline_hashes_at(root, "src/lib.rs").unwrap();

        // Weakening the assertion in the SEPARATE file — no edit to lib.rs
        // at all — must change the digest.
        fs::write(
            root.join("src/tests.rs"),
            "use super::*;\n#[test]\nfn adds() { assert!(true); }\n",
        )
        .unwrap();
        let weakened = inline_hashes_at(root, "src/lib.rs").unwrap();
        assert_ne!(baseline.cfg_test, weakened.cfg_test);

        // Restore the real assertion, then prove moving UNRELATED
        // implementation code around in lib.rs does not trip the gate: the
        // agent is entitled to edit that file.
        fs::write(
            root.join("src/tests.rs"),
            "use super::*;\n#[test]\nfn adds() { assert_eq!(add(1, 2), 3); }\n",
        )
        .unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "#[cfg(test)]\nmod tests;\n\npub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
        .unwrap();
        let moved = inline_hashes_at(root, "src/lib.rs").unwrap();
        assert_eq!(baseline.cfg_test, moved.cfg_test);
    }

    // C1: a `#[path = "..."]` attribute must be honoured, not just the
    // `<name>.rs` / `<name>/mod.rs` convention.
    #[test]
    fn a_path_attribute_redirects_module_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/test_support")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "#[cfg(test)]\n#[path = \"test_support/checks.rs\"]\nmod tests;\n",
        )
        .unwrap();
        fs::write(
            root.join("src/test_support/checks.rs"),
            "#[test]\nfn t() { assert_eq!(1 + 1, 2); }\n",
        )
        .unwrap();

        let baseline = inline_hashes_at(root, "src/lib.rs").unwrap();
        fs::write(
            root.join("src/test_support/checks.rs"),
            "#[test]\nfn t() { assert!(true); }\n",
        )
        .unwrap();
        let weakened = inline_hashes_at(root, "src/lib.rs").unwrap();
        assert_ne!(baseline.cfg_test, weakened.cfg_test);
    }

    // C1: a declared submodule can itself declare another. Once inside a
    // test-gated island, the nested declaration needs no `#[cfg(test)]` of
    // its own to count — it inherits the gate from its parent.
    #[test]
    fn a_nested_out_of_line_declaration_inside_a_test_module_is_hashed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/tests")).unwrap();
        fs::write(root.join("src/lib.rs"), "#[cfg(test)]\nmod tests;\n").unwrap();
        fs::write(root.join("src/tests.rs"), "mod helpers;\n").unwrap();
        fs::write(
            root.join("src/tests/helpers.rs"),
            "#[test]\nfn h() { assert_eq!(2 + 2, 4); }\n",
        )
        .unwrap();

        let baseline = inline_hashes_at(root, "src/lib.rs").unwrap();
        fs::write(
            root.join("src/tests/helpers.rs"),
            "#[test]\nfn h() { assert!(true); }\n",
        )
        .unwrap();
        let weakened = inline_hashes_at(root, "src/lib.rs").unwrap();
        assert_ne!(baseline.cfg_test, weakened.cfg_test);
    }

    // C1: absence must never look identical to presence. A declared module
    // whose file cannot be found is a hard error, not "no tests here".
    #[test]
    fn a_missing_out_of_line_module_file_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "#[cfg(test)]\nmod tests;\n").unwrap();
        // src/tests.rs deliberately not created.
        assert!(inline_hashes_at(root, "src/lib.rs").is_err());
    }

    // C1, via the real call `pipeline::eval` gate 5 uses: `verify_inline`
    // must fail closed the same way `inline_hashes_at` does, for a manifest
    // entry whose declared module file has disappeared since baseline.
    #[test]
    fn verify_inline_fails_closed_when_a_declared_module_file_disappears() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "#[cfg(test)]\nmod tests;\n").unwrap();
        fs::write(
            root.join("src/tests.rs"),
            "#[test]\nfn t() { assert_eq!(1, 1); }\n",
        )
        .unwrap();

        let mut m = Manifest::default();
        m.inline.insert(
            "src/lib.rs".into(),
            inline_hashes_at(root, "src/lib.rs").unwrap(),
        );

        fs::remove_file(root.join("src/tests.rs")).unwrap();
        assert_eq!(verify_inline(root, &m).unwrap(), vec!["src/lib.rs"]);
    }

    // I8: a doctest in an item position the walker used to miss entirely —
    // a trait method, a struct field, an enum variant, an associated const
    // inside an `impl`, an `extern` block, and a `union` — must be hashed
    // just as a doctest on a free function is.
    #[test]
    fn doctests_in_previously_missed_item_positions_are_hashed() {
        const SRC: &str = r#"
trait Greet {
    /// ```
    /// assert_eq!(1, 1);
    /// ```
    fn greet(&self);
}

struct S {
    /// ```
    /// assert_eq!(2, 2);
    /// ```
    field: i32,
}

enum E {
    /// ```
    /// assert_eq!(3, 3);
    /// ```
    Variant,
}

union U {
    /// ```
    /// assert_eq!(4, 4);
    /// ```
    field: i32,
}

struct T;
impl T {
    /// ```
    /// assert_eq!(5, 5);
    /// ```
    const N: i32 = 1;
}

extern "C" {
    /// ```
    /// assert_eq!(6, 6);
    /// ```
    fn f();
}
"#;
        let base = inline_hashes(SRC).unwrap().doc;
        for (needle, replacement) in [
            ("assert_eq!(1, 1);", "assert!(true);"),
            ("assert_eq!(2, 2);", "assert!(true);"),
            ("assert_eq!(3, 3);", "assert!(true);"),
            ("assert_eq!(4, 4);", "assert!(true);"),
            ("assert_eq!(5, 5);", "assert!(true);"),
            ("assert_eq!(6, 6);", "assert!(true);"),
        ] {
            let weakened = SRC.replace(needle, replacement);
            assert_ne!(
                base,
                inline_hashes(&weakened).unwrap().doc,
                "weakening the doctest touching {needle:?} must change the doc hash"
            );
        }
    }

    // A genuine cycle: `mod a;` (the top-level file itself) declares
    // `mod b;`, whose file declares `mod a;` right back — via `#[path]`, so
    // the identifier need not literally be "lib". This must still be
    // refused, not silently accepted now that revisiting a module is no
    // longer treated as cyclic on its own.
    #[test]
    fn a_genuine_cycle_is_still_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "#[cfg(test)]\nmod b;\n").unwrap();
        fs::write(root.join("src/b.rs"), "#[path = \"lib.rs\"]\nmod back;\n").unwrap();
        match inline_hashes_at(root, "src/lib.rs") {
            Err(FreezeError::Io(msg)) => assert!(msg.contains("cyclic"), "{msg}"),
            other => panic!("expected a cyclic-declaration error, got {other:?}"),
        }
    }

    // Finding 3: an ordinary diamond — the same test-support module declared
    // (without a body) from two different, non-cyclic parents — is
    // legitimate Rust and must hash successfully, not be misreported as a
    // cyclic module declaration merely because the traversal reaches it
    // twice.
    #[test]
    fn a_diamond_shaped_module_declaration_is_not_a_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        // lib.rs declares two test-gated modules, `a` and `b`, each of which
        // declares the SAME shared out-of-line module `shared` — reached by
        // two different, unrelated parents, not by any cycle.
        fs::write(
            root.join("src/lib.rs"),
            "#[cfg(test)]\nmod a;\n#[cfg(test)]\nmod b;\n",
        )
        .unwrap();
        // a.rs and b.rs both live directly under src/, the same directory as
        // shared.rs, so `#[path]` needs no `..` — it resolves against the
        // declaring file's own directory.
        fs::write(
            root.join("src/a.rs"),
            "#[path = \"shared.rs\"]\nmod shared;\n",
        )
        .unwrap();
        fs::write(
            root.join("src/b.rs"),
            "#[path = \"shared.rs\"]\nmod shared;\n",
        )
        .unwrap();
        fs::write(
            root.join("src/shared.rs"),
            "#[test]\nfn shared_check() { assert_eq!(1 + 1, 2); }\n",
        )
        .unwrap();

        let baseline =
            inline_hashes_at(root, "src/lib.rs").expect("a diamond must not error as a cycle");

        // And the diamond must still hash real content, not silently
        // collapse to an empty marker: weakening the shared assertion must
        // change the digest.
        fs::write(
            root.join("src/shared.rs"),
            "#[test]\nfn shared_check() { assert!(true); }\n",
        )
        .unwrap();
        let weakened = inline_hashes_at(root, "src/lib.rs").unwrap();
        assert_ne!(baseline.cfg_test, weakened.cfg_test);
    }

    // I8: `#[doc = include_str!("...")]` is an `Expr::Macro`, which the old
    // walker skipped outright, taking the included file's doctest content
    // with it into the empty-string digest. At minimum, a changed included
    // path must be detected — the macro's own token text is hashed.
    #[test]
    fn doc_include_str_path_changes_are_detected() {
        const A: &str = r#"
#[doc = include_str!("../doc/a.md")]
pub fn f() {}
"#;
        const B: &str = r#"
#[doc = include_str!("../doc/b.md")]
pub fn f() {}
"#;
        assert_ne!(inline_hashes(A).unwrap().doc, inline_hashes(B).unwrap().doc);
    }
}
