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
use std::collections::BTreeMap;
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
fn symlink_component(root: &Path, rel: &str) -> Result<Option<String>, FreezeError> {
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
    let file = syn::parse_file(source).map_err(|e| format!("parse source: {e}"))?;

    let mut cfg_test_digests = Vec::new();
    collect_cfg_test(&file.items, &mut cfg_test_digests);
    cfg_test_digests.sort();

    let mut docs = String::new();
    collect_docs(&file.attrs, &mut docs);
    collect_item_docs(&file.items, &mut docs);

    Ok(InlineHashes {
        cfg_test: sha256_bytes(cfg_test_digests.concat().as_bytes()),
        doc: sha256_bytes(docs.as_bytes()),
    })
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
fn collect_cfg_test(items: &[syn::Item], out: &mut Vec<String>) {
    for item in items {
        if item_attrs(item).iter().any(is_cfg_test) {
            out.push(sha256_bytes(item.to_token_stream().to_string().as_bytes()));
            continue;
        }
        if let syn::Item::Mod(m) = item {
            if let Some((_, inner)) = &m.content {
                collect_cfg_test(inner, out);
            }
        }
    }
}

/// Appends every doc-comment line's text.
fn collect_docs(attrs: &[syn::Attribute], out: &mut String) {
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &attr.meta {
            if let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
            {
                out.push_str(&s.value());
                out.push('\n');
            }
        }
    }
}

/// Walks every item that can carry a doc comment, recursing into modules and
/// impl blocks.
fn collect_item_docs(items: &[syn::Item], out: &mut String) {
    for item in items {
        match item {
            syn::Item::Fn(i) => collect_docs(&i.attrs, out),
            syn::Item::Struct(i) => collect_docs(&i.attrs, out),
            syn::Item::Enum(i) => collect_docs(&i.attrs, out),
            syn::Item::Trait(i) => collect_docs(&i.attrs, out),
            syn::Item::Const(i) => collect_docs(&i.attrs, out),
            syn::Item::Static(i) => collect_docs(&i.attrs, out),
            syn::Item::Type(i) => collect_docs(&i.attrs, out),
            syn::Item::Macro(i) => collect_docs(&i.attrs, out),
            syn::Item::Impl(i) => {
                collect_docs(&i.attrs, out);
                for it in &i.items {
                    if let syn::ImplItem::Fn(f) = it {
                        collect_docs(&f.attrs, out);
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

/// Reports which in-scope source files' inline tests differ from baseline.
///
/// A deleted or unreadable file counts as changed: its tests are certainly not
/// what they were.
pub fn verify_inline(repo_root: &Path, m: &Manifest) -> Result<Vec<String>, FreezeError> {
    let mut changed = Vec::new();
    for (rel, want) in &m.inline {
        if symlink_component(repo_root, rel)?.is_some() {
            changed.push(rel.clone());
            continue;
        }
        let path = safe_join(repo_root, rel)?;
        let Ok(text) = std::fs::read_to_string(&path) else {
            changed.push(rel.clone());
            continue;
        };
        match inline_hashes(&text) {
            Ok(got) if &got == want => {}
            // A file that stopped parsing has certainly changed, and the build
            // gate will report the syntax error properly a moment later.
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
}
