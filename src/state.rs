//! Transport-independent review state.
//!
//! Shared by the Tauri command layer and the TUI: the loaded AdhDiffMap +
//! parsed files, per-anchor comment threads, the viewed set, and the
//! sidecar load/persist (by default
//! `$XDG_STATE_HOME/adhdiff/<worktree_hash>/<diff_sha>.json`, or wherever
//! `--out` points). Everything here is synchronous; the caller owns
//! threading.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use adhdiff_shared::{AdhDiffMap, SavedComment};
use anyhow::{Context, Result, bail};
use sha2::Digest;

use crate::diff::FileDiff;

/// Format marker stored under the reserved "@version" key of the sidecar;
/// a file carrying any other value is not loaded.
const SIDECAR_VERSION: u64 = 2;

pub struct Inner {
    pub map: AdhDiffMap,
    pub files: Vec<FileDiff>,
    /// Per-anchor comment threads.
    /// Key: comment_key — `file:row` or `file:row-end_row` for ranges.
    pub comments: HashMap<String, Vec<SavedComment>>,
    /// Viewed hunks: `file:row_start` identity strings. Persisted in the
    /// same sidecar under the reserved "@viewed" key.
    pub viewed: BTreeSet<String>,
    /// The sidecar file holding comments and viewed marks.
    pub state_path: PathBuf,
    /// SHA-256 hex of the raw diff, used to namespace comment storage.
    pub diff_sha: String,
    /// Absolute path of the git/jj worktree the app was started in, used
    /// to resolve `e`/`open-in-editor` jumps. None outside a worktree
    /// (then the button is hidden).
    pub workdir_hint: Option<PathBuf>,
    /// The usable editor command resolved at startup; None when there is
    /// no worktree or `$VISUAL`/`$EDITOR` cannot be launched.
    pub editor: Option<String>,
    /// Rendered hunk-body markup by map index, filled by get_hunk_body /
    /// get_file_bodies on first request. Pins are baked into the markup,
    /// so comment mutations drop the file's entries.
    pub body_cache: HashMap<usize, String>,
}

/// The git/jj worktree containing cwd — the nearest ancestor holding a
/// `.git` or `.jj` — used for open-in-editor jumps and as the sidecar
/// key's canonical path. None outside a worktree.
pub fn detect_workdir() -> Option<PathBuf> {
    find_worktree_root(&std::env::current_dir().ok()?)
}

fn find_worktree_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|p| p.join(".git").exists() || p.join(".jj").exists())
        .map(Path::to_path_buf)
}

/// Sidecar file for a diff inside `store_dir`: `<diff-sha[..12]>.json`.
pub fn sidecar_path(store_dir: &Path, diff_sha: &str) -> PathBuf {
    store_dir.join(format!("{}.json", &diff_sha[..12]))
}

/// Resolve the sidecar file and load persisted state for `diff_sha`.
/// `--out` wins outright: an existing directory (or a path ending in `/`)
/// holds `<sha12>.json`, anything else is the state file itself.
/// Otherwise $XDG_STATE_HOME/adhdiff/<hash>/<sha12>.json with <hash>
/// keyed on the canonical worktree path (cwd when there is no worktree).
/// A missing file yields empty state. An unparseable file, or one from
/// an older adhdiff (no current "@version"), is renamed to
/// `<name>.corrupt-<ts>` before empty state proceeds — the next
/// successful persist would otherwise overwrite the user's original —
/// and the load warning string (last tuple element) tells the caller to
/// surface it.
pub fn load(
    diff_sha: &str,
    out: Option<&str>,
    workdir_hint: Option<&Path>,
) -> Result<(CommentThreads, BTreeSet<String>, PathBuf, Option<String>)> {
    let path = match out {
        Some(o) if o.ends_with('/') || Path::new(o).is_dir() => {
            sidecar_path(Path::new(o), diff_sha)
        }
        Some(o) => PathBuf::from(o),
        None => {
            let root = std::env::var_os("XDG_STATE_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
                .context("no XDG_STATE_HOME or HOME; pass --out")?;
            let key = workdir_hint
                .map(|p| p.to_path_buf())
                .or_else(|| std::env::current_dir().ok())
                .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
                .context("resolve cwd for state root")?;
            sidecar_path(
                &root
                    .join("adhdiff")
                    .join(&hex_sha256(key.to_string_lossy().as_bytes())[..16]),
                diff_sha,
            )
        }
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).context("create state dir")?;
    }
    let mut warn = None;
    let (comments, viewed) = match std::fs::read_to_string(&path) {
        Ok(s) => match serde_json::from_str::<serde_json::Value>(&s)
            .ok()
            .map(|v| check_stored_sha(&v, diff_sha, &path).map(|()| v))
            .transpose()?
            .and_then(split_sidecar)
        {
            Some(state) => state,
            None => {
                let aside = path.with_file_name(format!(
                    "{}.corrupt-{}-{}",
                    path.file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    now_unix(),
                    short_id()
                ));
                std::fs::rename(&path, &aside)
                    .with_context(|| format!("preserving unreadable sidecar {}", path.display()))?;
                warn = Some(format!(
                    "existing sidecar {} is unreadable or from an older adhdiff; preserved as {}, starting with empty state",
                    path.display(),
                    aside.display()
                ));
                Default::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
        Err(e) => return Err(e).with_context(|| format!("reading sidecar {}", path.display())),
    };
    Ok((comments, viewed, path, warn))
}

/// Refuse a sidecar whose stored diff digest is not the current diff's.
fn check_stored_sha(v: &serde_json::Value, diff_sha: &str, path: &Path) -> Result<()> {
    match v.get("@diff_sha").and_then(|x| x.as_str()) {
        Some(stored) if stored != diff_sha => bail!(
            "sidecar {} belongs to another diff: it stores {stored}, the current diff is {diff_sha}",
            path.display()
        ),
        _ => Ok(()),
    }
}

/// Per-anchor comment threads, keyed by `comment_key`.
pub type CommentThreads = HashMap<String, Vec<SavedComment>>;

/// Split a loaded sidecar into comment threads and the viewed set: the
/// reserved "@version" key must carry the current format, "@viewed"
/// holds viewed `file:row_start` identities, any other key is a
/// comment_key. `None` when the value is not a current-format sidecar.
pub fn split_sidecar(v: serde_json::Value) -> Option<(CommentThreads, BTreeSet<String>)> {
    let serde_json::Value::Object(mut m) = v else {
        return None;
    };
    if m.remove("@version").and_then(|v| v.as_u64()) != Some(SIDECAR_VERSION) {
        return None;
    }
    m.remove("@diff_sha");
    let viewed = match m.remove("@viewed") {
        Some(ids) => serde_json::from_value::<Vec<String>>(ids)
            .ok()?
            .into_iter()
            .collect(),
        None => BTreeSet::new(),
    };
    let mut comments = HashMap::new();
    for (k, val) in m {
        comments.insert(k, serde_json::from_value::<Vec<SavedComment>>(val).ok()?);
    }
    Some((comments, viewed))
}

/// Persist comments + viewed set to the sidecar atomically: write a
/// per-process unique sibling temp file and rename over the target, so
/// readers never observe a torn file and writers never share a temp.
pub fn persist_sidecar(g: &Inner) -> Result<()> {
    let p = &g.state_path;
    let mut v = serde_json::to_value(&g.comments)?;
    let obj = v
        .as_object_mut()
        .context("comments serialize to an object")?;
    obj.insert("@version".into(), SIDECAR_VERSION.into());
    obj.insert("@diff_sha".into(), g.diff_sha.clone().into());
    obj.insert("@viewed".into(), serde_json::to_value(&g.viewed)?);
    write_atomic(p, serde_json::to_string_pretty(&v)?.as_bytes())?;
    Ok(())
}

pub fn comment_key(file: &str, row: u32, end_row: Option<u32>) -> String {
    match end_row {
        Some(e) if e > row => format!("{}:{}-{}", file, row, e),
        _ => format!("{}:{}", file, row),
    }
}

/// Comments anchoring inside `file:[lo..=hi]` — a hunk's pane lists every
/// comment whose row falls in the hunk's row span.
pub fn comments_at(g: &Inner, file: &str, lo: u32, hi: u32) -> Vec<SavedComment> {
    let mut out: Vec<SavedComment> = g
        .comments
        .values()
        .flatten()
        .filter(|c| c.file == file && c.row >= lo && c.row <= hi)
        .cloned()
        .collect();
    out.sort_by_key(|c| (c.row, c.end_row.unwrap_or(c.row), c.ts_unix));
    out
}

/// Commented row ranges in `file` as (row, end_row_inclusive). At most one
/// comment occupies a row in a file, so each range owns its start row; a
/// comment without an end contributes (r, r).
pub fn row_pins_for(g: &Inner, file: &str) -> Vec<(u32, u32)> {
    let mut pins: Vec<(u32, u32)> = Vec::new();
    for cs in g.comments.values() {
        pins.extend(
            cs.iter()
                .filter(|c| c.file == file)
                .map(|c| (c.row, c.end_row.unwrap_or(c.row))),
        );
    }
    pins.sort_unstable();
    pins
}

pub fn hex_sha256(b: &[u8]) -> String {
    let mut h = sha2::Sha256::new();
    h.update(b);
    hex::encode(h.finalize())
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Process-unique 12-hex-char id: nanoseconds, pid and a counter, hashed.
pub fn short_id() -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut h);
    std::process::id().hash(&mut h);
    ID_COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut h);
    format!("{:016x}", h.finish())[..12].to_string()
}

/// A sibling of `p` that no other writer, in this process or another, uses.
pub fn unique_tmp_path(p: &Path) -> PathBuf {
    let mut tmp = p.to_path_buf().into_os_string();
    tmp.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        ID_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    PathBuf::from(tmp)
}

/// Write `bytes` to a unique sibling temp file, fsync it, and rename it over `p`.
pub fn write_atomic(p: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = unique_tmp_path(p);
    let res = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, p)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("adhdiff-state-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn worktree_root_is_the_nearest_ancestor_with_a_vcs_dir() {
        let root = scratch("walkup");
        std::fs::create_dir_all(root.join(".jj")).unwrap();
        let nested = root.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_worktree_root(&nested), Some(root.clone()));
        assert_eq!(find_worktree_root(&root), Some(root.clone()));
        std::fs::write(root.join("a/.git"), "gitdir: x").unwrap();
        assert_eq!(find_worktree_root(&nested), Some(root.join("a")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn out_directory_holds_sha12_json_and_a_file_path_is_used_verbatim() {
        let dir = scratch("out");
        let (_, _, p, w) = load(SHA, Some(dir.to_str().unwrap()), None).unwrap();
        assert_eq!(p, dir.join("0123456789ab.json"));
        assert!(w.is_none());

        let nested = format!("{}/new/sub/", dir.display());
        let (_, _, p, _) = load(SHA, Some(&nested), None).unwrap();
        assert_eq!(p, dir.join("new/sub/0123456789ab.json"));
        assert!(dir.join("new/sub").is_dir());

        let file = dir.join("deep/review.json");
        let (_, _, p, _) = load(SHA, Some(file.to_str().unwrap()), None).unwrap();
        assert_eq!(p, file);
        assert!(dir.join("deep").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_format_and_garbage_sidecars_are_set_aside_never_truncated() {
        let dir = scratch("aside");
        let path = dir.join("state.json");
        let old = r#"{"src/a.rs:10":[{"id":"x","file":"src/a.rs","start_line":10,"label_at_time":"","author":"human","body":"keep me","ts_unix":1}],"@viewed":["src/a.rs:10"]}"#;
        std::fs::write(&path, old).unwrap();
        let (comments, viewed, _, warn) = load(SHA, Some(path.to_str().unwrap()), None).unwrap();
        assert!(comments.is_empty() && viewed.is_empty());
        assert!(warn.unwrap().contains("older adhdiff"));
        assert!(!path.exists());
        let aside: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains(".corrupt-"))
            .collect();
        assert_eq!(aside.len(), 1);
        assert_eq!(std::fs::read_to_string(&aside[0]).unwrap(), old);

        std::fs::write(&path, "{not json").unwrap();
        let (_, _, _, warn) = load(SHA, Some(path.to_str().unwrap()), None).unwrap();
        assert!(warn.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sidecar_for_another_diff_is_refused_and_unreadable_is_an_error() {
        let dir = scratch("sha");
        let path = dir.join("state.json");
        std::fs::write(
            &path,
            format!(r#"{{"@version":2,"@diff_sha":"{}"}}"#, "f".repeat(32)),
        )
        .unwrap();
        let err = load(SHA, Some(path.to_str().unwrap()), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains(&"f".repeat(32)) && err.contains(SHA), "{err}");
        assert!(path.exists());

        // A path that cannot be read as a file is not "empty state".
        let as_dir = dir.join("is_dir");
        std::fs::create_dir(&as_dir).unwrap();
        let sub = format!("{}/", as_dir.display());
        assert!(load(SHA, Some(&sub), None).is_ok());
        std::fs::create_dir(as_dir.join("0123456789ab.json")).unwrap();
        assert!(load(SHA, Some(&sub), None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_ids_are_unique_in_a_tight_loop() {
        let ids: std::collections::HashSet<String> = (0..1000).map(|_| short_id()).collect();
        assert_eq!(ids.len(), 1000);
        assert!(ids.iter().all(|i| i.len() == 12));
    }

    #[test]
    fn partially_unreadable_sidecar_is_set_aside_not_dropped() {
        let dir = scratch("partial");
        let path = dir.join("state.json");
        let text = r#"{"@version":2,"a.rs:1":[{"id":"x","file":"a.rs","row":1,"label_at_time":"","author":"human","ts_unix":1}]}"#;
        std::fs::write(&path, text).unwrap();
        let (comments, _, _, warn) = load(SHA, Some(path.to_str().unwrap()), None).unwrap();
        assert!(comments.is_empty());
        assert!(warn.is_some());
        assert!(!path.exists());
        let aside: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains(".corrupt-"))
            .collect();
        assert_eq!(aside.len(), 1);
        assert_eq!(std::fs::read_to_string(&aside[0]).unwrap(), text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_writes_use_unique_temps_and_leave_none_behind() {
        let dir = scratch("atomic");
        let p = dir.join("s.json");
        assert_ne!(unique_tmp_path(&p), unique_tmp_path(&p));
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persisted_state_round_trips_through_load() {
        let dir = scratch("roundtrip");
        let path = dir.join("state.json");
        let c = SavedComment {
            id: "c1".into(),
            file: "src/a.rs".into(),
            row: 6,
            end_row: Some(8),
            label_at_time: "l".into(),
            author: "human".into(),
            body: "note".into(),
            ts_unix: 7,
        };
        let g = Inner {
            map: AdhDiffMap {
                title: String::new(),
                summary: String::new(),
                diff_sha256: String::new(),
                hunks: Vec::new(),
            },
            files: Vec::new(),
            comments: HashMap::from([(comment_key("src/a.rs", 6, Some(8)), vec![c.clone()])]),
            viewed: BTreeSet::from(["src/a.rs:5".to_string()]),
            state_path: path.clone(),
            diff_sha: SHA.into(),
            workdir_hint: None,
            editor: None,
            body_cache: HashMap::new(),
        };
        persist_sidecar(&g).unwrap();
        let (comments, viewed, _, warn) = load(SHA, Some(path.to_str().unwrap()), None).unwrap();
        assert!(warn.is_none());
        assert_eq!(comments["src/a.rs:6-8"], vec![c]);
        assert!(viewed.contains("src/a.rs:5"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
