//! Tauri window over the shared review state (`state::Inner`).
//!
//! The data layer: `get_map` hands the Leptos frontend a metadata-only
//! snapshot; highlighted hunk bodies are fetched on demand via
//! `get_hunk_body` (single, for lazy viewport prefetch) and
//! `get_file_bodies` (the comment-merge refresh slice), rendered once and
//! cached in `Inner::body_cache`. Comment/viewed mutations persist to the
//! sidecar and return only the affected slice (single window, self-writes
//! only — the reply IS the update, no events). Commands are thin
//! `#[tauri::command]` wrappers over the pure `*_now` fns below so tests
//! exercise the same logic without a Tauri runtime.

use std::sync::{Mutex, MutexGuard, PoisonError};

use adhdiff_shared::{
    AdhDiffMap, FileBodies, FileComments, HunkBody, MapPayload, NewComment, SavedComment,
    ViewedState,
};
use anyhow::{Context, Result};
use tauri::{Manager, State};

use crate::diff::{find_raw_for, parse_unified, row_to_new_line};
use crate::outline::{
    file_bodies, hunk_body, hunk_identity, invalidate_file_bodies, map_payload, outline_position,
};
use crate::state::{
    self, Inner, comment_key, comments_at, hex_sha256, now_unix, persist_sidecar, short_id,
};

// ---------- commands ----------

/// Lock the shared state, recovering the guard if a panicked command
/// poisoned the mutex: the data is plain review state and stays usable.
fn lock(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[tauri::command]
fn get_map(state: State<'_, Mutex<Inner>>) -> MapPayload {
    map_payload(&lock(&state))
}

#[tauri::command]
fn get_hunk_body(map_idx: usize, state: State<'_, Mutex<Inner>>) -> Result<HunkBody, String> {
    hunk_body_now(&mut lock(&state), map_idx).ok_or_else(|| "no such hunk".to_string())
}

#[tauri::command]
fn get_file_bodies(file: String, state: State<'_, Mutex<Inner>>) -> FileBodies {
    file_bodies_now(&mut lock(&state), &file)
}

#[tauri::command]
fn add_comment(c: NewComment, state: State<'_, Mutex<Inner>>) -> Result<FileComments, String> {
    add_comment_now(&mut lock(&state), c).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_comment(
    file: String,
    row: u32,
    state: State<'_, Mutex<Inner>>,
) -> Result<FileComments, String> {
    delete_comment_now(&mut lock(&state), &file, row).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_viewed(
    file: String,
    row: u32,
    state: State<'_, Mutex<Inner>>,
) -> Result<ViewedState, String> {
    toggle_viewed_now(&mut lock(&state), &file, row).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_in_editor(
    path: String,
    row: Option<u32>,
    state: State<'_, Mutex<Inner>>,
) -> Result<(), String> {
    open_in_editor_now(&lock(&state), &path, row)
}

/// Whole-webview zoom via wry's native zoom (WebKitGTK set_zoom_level —
/// scales text, canvas and HiDPI alike). The frontend owns the factor and
/// its persistence; this just applies it.
#[tauri::command]
fn set_zoom(factor: f64, webview: tauri::Webview) -> Result<(), String> {
    webview.set_zoom(factor).map_err(|e| e.to_string())
}

// ---------- pure command logic ----------

/// Render (or serve from cache) one hunk's body by map index.
pub fn hunk_body_now(g: &mut Inner, map_idx: usize) -> Option<HunkBody> {
    let oi = outline_position(g, map_idx)?;
    hunk_body(g, map_idx, oi)
}

/// Render (fresh after a comment mutation — the mutation invalidated the
/// file's cache entries) every body of one file.
pub fn file_bodies_now(g: &mut Inner, file: &str) -> FileBodies {
    file_bodies(g, file)
}

pub fn add_comment_now(g: &mut Inner, c: NewComment) -> Result<FileComments> {
    let end_row = c.end_row.filter(|&e| e > c.row);
    let key = comment_key(&c.file, c.row, end_row);
    let comment = SavedComment {
        id: short_id(),
        file: c.file.clone(),
        row: c.row,
        end_row,
        label_at_time: g
            .map
            .hunks
            .iter()
            .find(|h| h.row_start <= c.row && c.row <= h.row_end)
            .map(|h| h.label.clone())
            .unwrap_or_default(),
        author: "human".into(),
        body: c.body,
        ts_unix: now_unix(),
    };
    // At most one comment per row: drop every comment in this file whose
    // occupied range intersects the new one (whole replacement), then store
    // the new comment under its own key.
    let new_end = end_row.unwrap_or(c.row);
    let previous = g.comments.clone();
    g.comments.retain(|_, cs| {
        !cs.iter().any(|c| {
            c.file == comment.file && c.row <= new_end && c.end_row.unwrap_or(c.row) >= comment.row
        })
    });
    g.comments.entry(key).or_default().push(comment);
    if let Err(e) = persist_sidecar(g) {
        g.comments = previous;
        return Err(e);
    }
    invalidate_file_bodies(g, &c.file);
    Ok(file_comments(g, &c.file))
}

pub fn delete_comment_now(g: &mut Inner, file: &str, row: u32) -> Result<FileComments> {
    g.comments.retain(|_, cs| {
        cs.retain(|c| !(c.file == file && c.row == row));
        !cs.is_empty()
    });
    invalidate_file_bodies(g, file);
    persist_sidecar(g)?;
    Ok(file_comments(g, file))
}

pub fn toggle_viewed_now(g: &mut Inner, file: &str, row: u32) -> Result<ViewedState> {
    let key = hunk_identity(file, row);
    if !g.viewed.remove(&key) {
        g.viewed.insert(key);
    }
    persist_sidecar(g)?;
    Ok(viewed_state(g))
}

/// Spawn `$EDITOR` for the new-file line behind diff row `row`, in the
/// file of the raw hunk containing it (`file` and line 1 when the row is
/// unknown). Errors without a workdir or when the editor cannot start.
pub fn open_in_editor_now(g: &Inner, file: &str, row: Option<u32>) -> Result<(), String> {
    let Some(wd) = &g.workdir_hint else {
        return Err("not inside a git/jj worktree".to_string());
    };
    let hit =
        row.and_then(|r| find_raw_for(&g.files, r).map(|(f, raw)| (f, row_to_new_line(raw, r))));
    let (path, line) = match hit {
        Some((f, line)) => (f.path(), line),
        None => (file, 1),
    };
    crate::editor::open_in_editor(path, line, wd).map(|_| ())
}

/// The full viewed key set (`file:row_start`), sorted — hunks are small,
/// shipping the whole set is cheap.
fn viewed_state(g: &Inner) -> ViewedState {
    ViewedState {
        viewed: g.viewed.iter().cloned().collect(),
    }
}

/// One file's full comment list, sorted like a hunk's pane would show it.
fn file_comments(g: &Inner, file: &str) -> FileComments {
    FileComments {
        file: file.to_string(),
        comments: comments_at(g, file, 0, u32::MAX),
    }
}

/// Build the shared review state for both `run_app` and the tests.
fn build_inner(map: AdhDiffMap, diff: &[u8], out: Option<String>) -> Result<Inner> {
    let files = parse_unified(diff)?;
    let diff_sha = hex_sha256(diff);

    let workdir_hint = state::detect_workdir();
    let (comments, viewed, state_path, load_warn) =
        state::load(&diff_sha, out.as_deref(), workdir_hint.as_deref())?;
    if let Some(w) = &load_warn {
        eprintln!("adhdiff gui: {w}");
    }

    Ok(Inner {
        map,
        files,
        comments,
        viewed,
        state_path,
        diff_sha,
        workdir_hint,
        body_cache: std::collections::HashMap::new(),
    })
}

pub fn run_app(map: AdhDiffMap, diff: &[u8], out: Option<String>) -> Result<()> {
    let inner = build_inner(map, diff, out)?;
    eprintln!("adhdiff: review state: {}", inner.state_path.display());
    match std::fs::remove_file(inner.state_path.with_file_name("REVIEW_DONE")) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("adhdiff: failed to remove stale REVIEW_DONE: {e}"),
    }
    tauri::Builder::default()
        .manage(Mutex::new(inner))
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                let st = window.state::<Mutex<Inner>>();
                let g = lock(&st);
                let comments: usize = g.comments.values().map(Vec::len).sum();
                let viewed = g.viewed.len();
                let done = serde_json::json!({
                    "diff_sha12": &g.diff_sha[..12],
                    "closed_at_unix": now_unix(),
                    "comments": comments,
                    "viewed": viewed,
                });
                if let Err(e) =
                    std::fs::write(g.state_path.with_file_name("REVIEW_DONE"), done.to_string())
                {
                    eprintln!("adhdiff: failed to write REVIEW_DONE: {e}");
                }
                eprintln!(
                    "adhdiff: review closed: {} (comments={comments}, viewed={viewed})",
                    g.state_path.display()
                );
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_map,
            get_hunk_body,
            get_file_bodies,
            add_comment,
            delete_comment,
            set_viewed,
            open_in_editor,
            set_zoom,
        ])
        .run(tauri::generate_context!())
        .context("running tauri app")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_inner(out: &std::path::Path) -> Inner {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/segments");
        let map: AdhDiffMap =
            serde_json::from_slice(&std::fs::read(format!("{root}/demo.map.json")).unwrap())
                .unwrap();
        let diff = std::fs::read(format!("{root}/demo.diff")).unwrap();
        build_inner(map, &diff, Some(out.to_string_lossy().into())).unwrap()
    }

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("adhdiff-app-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn get_map_builds_full_payload_from_fixture() {
        let dir = scratch_dir("payload");
        let inner = fixture_inner(&dir);
        let payload = map_payload(&inner);
        assert_eq!(payload.title, inner.map.title);
        // Six map entries tile three raw hunks: one outline item each.
        assert_eq!(inner.map.hunks.len(), 6);
        assert_eq!(payload.hunks.len(), 3);
        assert_eq!(payload.outline.len(), 3);
        assert_eq!(payload.diff_sha12.len(), 12);
        assert_eq!(payload.diff_sha12, inner.map.diff_sha256[..12]);
        // Metadata-only: every hunk ships its body's row count so the
        // frontend can reserve space before the markup arrives.
        assert_eq!(
            payload
                .hunks
                .iter()
                .map(|h| h.body_rows)
                .collect::<Vec<_>>(),
            vec![180, 11, 5]
        );
        let big = &payload.hunks[0];
        assert_eq!(
            (big.file.as_str(), big.row_start, big.row_end),
            ("src/big.rs", 6, 185)
        );
        assert_eq!(big.segments.len(), 3);
        assert_eq!(big.segments[1].row_start, 146);
        assert_eq!(big.segments[1].tier, 1);
        assert_eq!(big.supports, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hunk_bodies_render_once_cache_and_repin_after_comment() {
        let dir = scratch_dir("bodies");
        let mut inner = fixture_inner(&dir);
        let payload = map_payload(&inner);
        let h = &payload.hunks[0];
        let (mi, file, row) = (h.map_idx, h.file.clone(), h.row_start);
        let oi = outline_position(&inner, mi).unwrap();
        assert_eq!(oi, 0);

        assert!(inner.body_cache.is_empty());
        let body = hunk_body_now(&mut inner, mi).unwrap();
        assert_eq!(body.idx, oi);
        assert!(body.body_html.contains("hunk-body"));
        assert!(body.comments.is_empty());
        // Second fetch serves the cache verbatim (no re-render).
        assert_eq!(inner.body_cache.len(), 1);
        let cached = inner.body_cache.get(&mi).unwrap().clone();
        assert_eq!(cached, body.body_html);
        assert_eq!(hunk_body_now(&mut inner, mi).unwrap().body_html, cached);

        // A comment on a row bakes pins into that file's bodies and
        // merges back the comment list.
        add_comment_now(
            &mut inner,
            NewComment {
                file: file.clone(),
                row,
                end_row: None,
                body: "pin me".into(),
            },
        )
        .unwrap();
        assert!(
            inner.body_cache.is_empty(),
            "comment invalidates the file's bodies"
        );
        let fb = file_bodies_now(&mut inner, &file);
        assert_eq!(fb.file, file);
        assert!(
            fb.bodies
                .iter()
                .any(|b| b.body_html.contains("line-commented"))
        );
        let ours = fb.bodies.iter().find(|b| b.map_idx == mi).unwrap();
        assert_eq!(ours.comments.len(), 1);
        assert_eq!(ours.comments[0].body, "pin me");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_comment_persists_and_reads_back_from_fresh_inner() {
        let dir = scratch_dir("comment");
        let mut inner = fixture_inner(&dir);
        let payload = map_payload(&inner);
        let h = &payload.hunks[1];
        let file = h.file.clone();
        // Row 193 is the removed line inside lib.rs's first segment.
        let label = inner.map.hunks[3].label.clone();

        let fc = add_comment_now(
            &mut inner,
            NewComment {
                file: file.clone(),
                row: 193,
                end_row: None,
                body: "looks racy".into(),
            },
        )
        .unwrap();
        assert_eq!(fc.file, file);
        assert_eq!(fc.comments.len(), 1);
        assert_eq!(fc.comments[0].body, "looks racy");
        assert_eq!(fc.comments[0].author, "human");
        assert_eq!(fc.comments[0].row, 193);
        assert_eq!(fc.comments[0].label_at_time, label);
        assert!(fc.comments[0].end_row.is_none());

        // Persisted under its comment_key; a fresh Inner over the same
        // state dir reads the thread back untouched.
        let reloaded = fixture_inner(&dir);
        assert!(reloaded.comments.contains_key("src/lib.rs:193"));
        let fc2 = file_comments(&reloaded, &file);
        assert_eq!(fc2.comments.len(), 1);
        assert_eq!(fc2.comments[0].id, fc.comments[0].id);
        assert_eq!(fc2.comments[0].body, "looks racy");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overlapping_comment_replaces_and_delete_clears() {
        let dir = scratch_dir("overlap");
        let mut inner = fixture_inner(&dir);
        let add = |inner: &mut Inner, row, end_row, body: &str| {
            add_comment_now(
                inner,
                NewComment {
                    file: "src/lib.rs".into(),
                    row,
                    end_row,
                    body: body.into(),
                },
            )
            .unwrap()
        };
        add(&mut inner, 192, Some(194), "range");
        let fc = add(&mut inner, 194, None, "single");
        assert_eq!(fc.comments.len(), 1);
        assert_eq!(fc.comments[0].body, "single");
        assert_eq!(inner.comments.len(), 1);

        let fc = delete_comment_now(&mut inner, "src/lib.rs", 194).unwrap();
        assert!(fc.comments.is_empty());
        assert!(inner.comments.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_persist_leaves_memory_unchanged() {
        let dir = scratch_dir("rollback");
        let mut inner = fixture_inner(&dir);
        let add = |inner: &mut Inner, row, body: &str| {
            add_comment_now(
                inner,
                NewComment {
                    file: "src/lib.rs".into(),
                    row,
                    end_row: None,
                    body: body.into(),
                },
            )
        };
        add(&mut inner, 193, "kept").unwrap();
        let good = inner.state_path.clone();
        inner.state_path = dir.join("missing/dir/state.json");
        assert!(add(&mut inner, 193, "lost").is_err());
        assert_eq!(inner.comments.len(), 1);
        assert_eq!(inner.comments["src/lib.rs:193"][0].body, "kept");
        inner.state_path = good;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn poisoned_state_lock_is_recovered() {
        let dir = scratch_dir("poison");
        let m = Mutex::new(fixture_inner(&dir));
        std::thread::scope(|s| {
            let _ = s
                .spawn(|| {
                    let _g = m.lock().unwrap();
                    panic!("poison");
                })
                .join();
        });
        assert!(m.is_poisoned());
        assert_eq!(lock(&m).map.hunks.len(), 6);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_in_editor_without_a_worktree_reports_an_error() {
        let dir = scratch_dir("noeditor");
        let mut inner = fixture_inner(&dir);
        inner.workdir_hint = None;
        assert!(open_in_editor_now(&inner, "src/lib.rs", Some(193)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_viewed_toggles_and_persists_both_directions() {
        let dir = scratch_dir("viewed");
        let mut inner = fixture_inner(&dir);
        let payload = map_payload(&inner);
        let (file, row) = (payload.hunks[0].file.clone(), payload.hunks[0].row_start);
        let identity = "src/big.rs:6".to_string();

        let on = toggle_viewed_now(&mut inner, &file, row).unwrap();
        assert!(on.viewed.contains(&identity));
        assert!(map_payload(&inner).hunks[0].viewed);
        // On-state survives a reload from the sidecar …
        let mut reloaded = fixture_inner(&dir);
        assert!(reloaded.viewed.contains(&identity));

        // … and toggling back off persists too.
        let off = toggle_viewed_now(&mut reloaded, &file, row).unwrap();
        assert!(!off.viewed.contains(&identity));
        let reloaded2 = fixture_inner(&dir);
        assert!(!reloaded2.viewed.contains(&identity));
        assert!(!map_payload(&reloaded2).hunks[0].viewed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_path_names_the_state_file() {
        let dir = scratch_dir("out");
        let file = dir.join("custom/review.json");
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/segments");
        let map: AdhDiffMap =
            serde_json::from_slice(&std::fs::read(format!("{root}/demo.map.json")).unwrap())
                .unwrap();
        let diff = std::fs::read(format!("{root}/demo.diff")).unwrap();
        let mut inner = build_inner(map, &diff, Some(file.to_string_lossy().into())).unwrap();
        assert_eq!(inner.state_path, file);
        toggle_viewed_now(&mut inner, "src/big.rs", 6).unwrap();
        assert!(file.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
