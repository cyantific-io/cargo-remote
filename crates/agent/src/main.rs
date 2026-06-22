//! `rustle-agent` — the remote half of the sync engine.
//!
//! The host ships this crate's *source* into its sandbox under the remote build dir and compiles
//! it there with a bare `rustc` (no cargo, no registry, no network — hence the strict std-only,
//! zero-dependency rule). The agent then runs over an SSH exec channel, speaking the binary
//! [`proto`]col on stdin/stdout.
//!
//! Its job: given the host's local manifest, reconcile the remote build tree in a single
//! round-trip — create needed directories, recreate changed symlinks, prune extraneous entries —
//! and report back the files whose *contents* the host must upload (over SFTP). The host's
//! `O(dirs)` listing + `O(stale)` prune + `O(dirs)` mkdir round-trips collapse into this one
//! request, because all of that set-arithmetic now happens locally on the remote filesystem.
//!
//! Stdout carries protocol frames ONLY; all diagnostics go to stderr (the exec channel keeps the
//! two streams separate), or the wire would corrupt.

// `proto` is the canonical wire contract shared with the host; the agent only exercises one
// direction of it (decode requests, encode responses), so the other half is dead here.
#[allow(dead_code)]
mod proto;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use proto::{FileEntry, LinkEntry, Request, Response};

/// Bumped alongside behavioural changes; reported in the handshake.
const AGENT_VERSION: u32 = 1;

/// The reconciled remote tree: (rel → size+mtime) files and (rel → target) symlinks.
type Tree = (HashMap<String, (u64, u32)>, HashMap<String, String>);

fn main() {
    // The build root is argv[1]; everything is resolved beneath it (no global chdir, so the
    // engine stays a pure function of (root, request) — and unit-testable).
    let root = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".to_string()));

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut writer = BufWriter::new(stdout.lock());

    // Reads requests until EOF / closed channel (`read_frame` errs), or an explicit `Bye`.
    while let Ok(frame) = proto::read_frame(&mut reader) {
        let request = match Request::decode(&frame) {
            Some(request) => request,
            None => {
                let _ = reply(&mut writer, &Response::Error {
                    message: "malformed request frame".to_string(),
                });
                break;
            }
        };
        let response = match request {
            Request::Hello { .. } => Response::HelloOk {
                agent: AGENT_VERSION,
                proto: proto::PROTOCOL_VERSION,
            },
            Request::Plan {
                include_hidden,
                prune,
                excludes,
                files,
                links,
            } => apply_plan(&root, include_hidden, prune, &excludes, &files, &links),
            Request::Bye => break,
        };
        if reply(&mut writer, &response).is_err() {
            break;
        }
    }
}

fn reply(writer: &mut impl Write, response: &Response) -> io::Result<()> {
    proto::write_frame(writer, &response.encode())
}

/// Reconcile the remote tree under `root` against the host's manifest, returning the upload
/// worklist. Mirrors the host's transfer semantics exactly (size+mtime quick-check, top-level
/// path excludes, hidden-at-any-depth, symlinks preserved, `target/` never walked).
fn apply_plan(
    root: &Path,
    include_hidden: bool,
    prune: bool,
    excludes: &[String],
    files: &[FileEntry],
    links: &[LinkEntry],
) -> Response {
    let (remote_files, remote_links) = match walk(root, excludes, include_hidden) {
        Ok(state) => state,
        Err(e) => {
            return Response::Error {
                message: format!("failed to read remote build tree: {e}"),
            }
        }
    };

    let mut ensured: HashSet<String> = HashSet::new();
    let mut created_dirs = 0u32;
    let mut symlinks = 0u32;
    let mut uploads = Vec::new();

    // Files whose size+mtime differ from (or are absent on) the remote must be re-uploaded; their
    // parent directories are created now so the host's SFTP write lands in a ready tree.
    for f in files {
        let changed = match remote_files.get(&f.rel) {
            Some((size, mtime)) => *size != f.size || *mtime != f.mtime,
            None => true,
        };
        if changed {
            ensure_parent(root, &f.rel, &mut ensured, &mut created_dirs);
            uploads.push(f.rel.clone());
        }
    }

    // Recreate any symlink whose target differs from (or is absent on) the remote.
    for l in links {
        if remote_links.get(&l.rel).map(String::as_str) == Some(l.target.as_str()) {
            continue;
        }
        ensure_parent(root, &l.rel, &mut ensured, &mut created_dirs);
        let path = root.join(&l.rel);
        let _ = fs::remove_file(&path); // clear a stale file or outdated link
        if make_symlink(&l.target, &path).is_ok() {
            symlinks += 1;
        }
    }

    // Prune anything on the remote that the local tree no longer has (never directories, and
    // never excluded subtrees like `target/` — those were never walked).
    let mut pruned = 0u32;
    if prune {
        let local: HashSet<&str> = files
            .iter()
            .map(|f| f.rel.as_str())
            .chain(links.iter().map(|l| l.rel.as_str()))
            .collect();
        for rel in remote_files.keys().chain(remote_links.keys()) {
            if !local.contains(rel.as_str()) && fs::remove_file(root.join(rel)).is_ok() {
                pruned += 1;
            }
        }
    }

    Response::Worklist {
        uploads,
        created_dirs,
        pruned,
        symlinks,
    }
}

/// Recursively read the tree under `root` into (rel → size+mtime) files and (rel → target)
/// symlinks. Symlinks are recorded, not followed; excluded subtrees are skipped without
/// descending (so `target/` is never traversed). A missing root yields empty maps.
fn walk(root: &Path, excludes: &[String], include_hidden: bool) -> io::Result<Tree> {
    let mut files = HashMap::new();
    let mut links = HashMap::new();
    if root.exists() {
        walk_dir(root, "", excludes, include_hidden, &mut files, &mut links)?;
    }
    Ok((files, links))
}

fn walk_dir(
    dir: &Path,
    rel_prefix: &str,
    excludes: &[String],
    include_hidden: bool,
    files: &mut HashMap<String, (u64, u32)>,
    links: &mut HashMap<String, String>,
) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if rel_prefix.is_empty() {
            name
        } else {
            format!("{rel_prefix}/{name}")
        };
        if is_excluded(&rel, excludes, include_hidden) {
            continue;
        }
        // `file_type()` comes from the dirent and does NOT follow symlinks.
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            if let Ok(target) = fs::read_link(entry.path()) {
                links.insert(rel, target.to_string_lossy().into_owned());
            }
        } else if file_type.is_dir() {
            walk_dir(&entry.path(), &rel, excludes, include_hidden, files, links)?;
        } else if file_type.is_file() {
            let metadata = entry.metadata()?;
            files.insert(rel, (metadata.len(), mtime_secs(&metadata)));
        }
    }
    Ok(())
}

/// Same exclusion semantics as the host: hidden components are excluded at any depth (unless
/// `include_hidden`); other excludes are top-level path prefixes (`target` ⇒ `target` and
/// `target/...`, but not `crates/target.rs`). The `.*` sentinel is handled by the hidden check.
fn is_excluded(rel: &str, excludes: &[String], include_hidden: bool) -> bool {
    if !include_hidden && rel.split('/').any(|c| c.starts_with('.')) {
        return true;
    }
    for ex in excludes {
        if ex == ".*" {
            continue;
        }
        if rel == ex
            || (rel.len() > ex.len()
                && rel.starts_with(ex.as_str())
                && rel.as_bytes()[ex.len()] == b'/')
        {
            return true;
        }
    }
    false
}

fn mtime_secs(metadata: &fs::Metadata) -> u32 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}

/// Create the parent directory chain of `rel` (relative to `root`), component by component,
/// deduped across the whole plan via `ensured`. Counts directories actually created.
fn ensure_parent(root: &Path, rel: &str, ensured: &mut HashSet<String>, created: &mut u32) {
    let Some(idx) = rel.rfind('/') else {
        return; // top-level entry; root already exists
    };
    let dir = &rel[..idx];
    let mut prefix = String::new();
    for part in dir.split('/') {
        if part.is_empty() {
            continue;
        }
        if prefix.is_empty() {
            prefix = part.to_string();
        } else {
            prefix.push('/');
            prefix.push_str(part);
        }
        if ensured.insert(prefix.clone()) && fs::create_dir(root.join(&prefix)).is_ok() {
            *created += 1;
        }
    }
}

#[cfg(unix)]
fn make_symlink(target: &str, path: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(not(unix))]
fn make_symlink(_target: &str, _path: &Path) -> io::Result<()> {
    // Remote build hosts are unix; this keeps the agent compiling on a dev's non-unix box.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        // Tags are unique per test; pid makes it unique per run.
        let dir = std::env::temp_dir()
            .join(format!("rustle-agent-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn plan_uploads_changed_prunes_stale_and_recreates_links() {
        let root = tmp("plan");
        // Existing remote tree.
        fs::write(root.join("foo.rs"), b"old").unwrap(); // will be reported changed (size differs)
        fs::write(root.join("stale.rs"), b"x").unwrap(); // absent locally → pruned
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("target/debug/junk"), b"big").unwrap(); // excluded → never pruned

        let excludes = vec!["target".to_string(), ".*".to_string()];
        let files = vec![
            FileEntry { rel: "foo.rs".into(), size: 999, mtime: 1 }, // differs from on-disk "old"
            FileEntry { rel: "src/bar.rs".into(), size: 10, mtime: 1 }, // absent → upload + mkdir
        ];
        let links = vec![LinkEntry { rel: "link.rs".into(), target: "foo.rs".into() }];

        let resp = apply_plan(&root, false, true, &excludes, &files, &links);
        let Response::Worklist { uploads, pruned, symlinks, .. } = resp else {
            panic!("expected worklist, got {resp:?}");
        };

        // foo.rs (changed) and src/bar.rs (absent) are uploads; the new parent dir was created.
        assert!(uploads.contains(&"foo.rs".to_string()));
        assert!(uploads.contains(&"src/bar.rs".to_string()));
        assert!(root.join("src").is_dir(), "parent dir pre-created for the host's upload");

        // stale.rs pruned; excluded target/ subtree untouched.
        assert!(!root.join("stale.rs").exists());
        assert!(root.join("target/debug/junk").exists());
        assert_eq!(pruned, 1);

        // symlink recreated.
        assert_eq!(symlinks, 1);
        assert_eq!(fs::read_link(root.join("link.rs")).unwrap().to_str(), Some("foo.rs"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unchanged_file_is_not_an_upload() {
        let root = tmp("unchanged");
        fs::write(root.join("keep.rs"), b"hello").unwrap();
        let metadata = fs::metadata(root.join("keep.rs")).unwrap();
        let manifest = vec![FileEntry {
            rel: "keep.rs".into(),
            size: metadata.len(),
            mtime: mtime_secs(&metadata),
        }];

        let resp = apply_plan(&root, false, true, &["target".into()], &manifest, &[]);
        let Response::Worklist { uploads, pruned, .. } = resp else {
            panic!("expected worklist");
        };
        assert!(uploads.is_empty(), "matching size+mtime must not re-upload");
        assert_eq!(pruned, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_root_yields_all_uploads_no_error() {
        let root = std::env::temp_dir().join(format!("rustle-agent-absent-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let files = vec![FileEntry { rel: "a.rs".into(), size: 1, mtime: 1 }];
        let resp = apply_plan(&root, false, true, &["target".into()], &files, &[]);
        match resp {
            Response::Worklist { uploads, .. } => assert_eq!(uploads, vec!["a.rs".to_string()]),
            other => panic!("expected worklist, got {other:?}"),
        }
    }
}
