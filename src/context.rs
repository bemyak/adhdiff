//! Worktree lines surrounding a raw hunk: up to `Inner::context` lines
//! above and below, read from the new-side file and shown dimmed next to
//! the hunk's own rows.

use std::collections::HashMap;

use crate::diff::{FileDiff, RawHunk};
use crate::state::Inner;

/// New-side `(line number, text)` rows around one hunk.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExtraCtx {
    pub before: Vec<(u32, String)>,
    pub after: Vec<(u32, String)>,
}

/// Worktree files already read during one request, by diff path; `None`
/// marks a file that could not be used.
pub type FileLines = HashMap<String, Option<Vec<String>>>;

/// The text column of extra rows.
pub fn texts(rows: &[(u32, String)]) -> Vec<&str> {
    rows.iter().map(|(_, t)| t.as_str()).collect()
}

fn file_lines<'c>(g: &Inner, f: &FileDiff, cache: &'c mut FileLines) -> Option<&'c [String]> {
    cache
        .entry(f.path().to_string())
        .or_insert_with(|| {
            let wd = g.workdir_hint.as_ref()?;
            if f.new_path == "/dev/null" || !crate::editor::openable(&f.new_path, wd) {
                return None;
            }
            let text = std::fs::read_to_string(wd.join(&f.new_path)).ok()?;
            Some(text.lines().map(str::to_string).collect())
        })
        .as_deref()
}

/// New-side extent `(first, last)` of a hunk; `last == first - 1` when the
/// hunk keeps no new-side row.
fn new_extent(h: &RawHunk) -> (u32, u32) {
    let count = new_side(h).count() as u32;
    let first = if count == 0 {
        h.new_start.saturating_add(1)
    } else {
        h.new_start
    };
    (first, (first + count).saturating_sub(1))
}

/// Body rows present on the new side: `(is_context, text)`.
fn new_side(h: &RawHunk) -> impl Iterator<Item = (bool, &str)> {
    h.body.lines().filter_map(|l| {
        if let Some(rest) = l.strip_prefix('+') {
            Some((false, rest))
        } else if l.starts_with(['-', '\\']) {
            None
        } else {
            Some((true, l.strip_prefix(' ').unwrap_or(l)))
        }
    })
}

/// The extra rows for `raw`, or none when `Inner::context` is 0, the file
/// is not a readable regular file under the worktree, or the file no longer
/// matches the hunk's own context rows (edited since the diff was cut).
/// Rows never reach into the gap shared with a neighbouring hunk of the
/// same file: the gap is split between the two.
pub fn extra_context(g: &Inner, f: &FileDiff, raw: &RawHunk, cache: &mut FileLines) -> ExtraCtx {
    let none = ExtraCtx::default();
    if g.context == 0 {
        return none;
    }
    let Some(lines) = file_lines(g, f, cache) else {
        return none;
    };
    let line = |n: u32| {
        let at = usize::try_from(n.checked_sub(1)?).ok()?;
        lines.get(at).map(|s| s.trim_end_matches('\r'))
    };
    let (first, last) = new_extent(raw);
    if last as usize > lines.len() {
        return none;
    }
    let rows: Vec<(bool, &str)> = new_side(raw).collect();
    let head = rows.iter().take_while(|r| r.0).count();
    let tail = rows.iter().rev().take_while(|r| r.0).count();
    let matches = |i: usize| line(first + i as u32) == Some(rows[i].1.trim_end_matches('\r'));
    if !(0..head).all(matches) || !(rows.len().saturating_sub(tail)..rows.len()).all(matches) {
        return none;
    }

    let mut room_before = first.saturating_sub(1);
    let mut room_after = (lines.len() as u32).saturating_sub(last);
    let (mut gap_before, mut gap_after) = (None, None);
    for h in f.hunks.iter().filter(|h| h.row_start != raw.row_start) {
        let (nf, nl) = new_extent(h);
        if nl < first {
            let gap = first.saturating_sub(nl + 1);
            gap_before = Some(gap_before.map_or(gap, |g: u32| g.min(gap)));
        } else if nf > last {
            let gap = nf.saturating_sub(last + 1);
            gap_after = Some(gap_after.map_or(gap, |g: u32| g.min(gap)));
        }
    }
    if let Some(gap) = gap_before {
        room_before = room_before.min(gap / 2);
    }
    if let Some(gap) = gap_after {
        room_after = room_after.min(gap.div_ceil(2));
    }
    let nb = g.context.min(room_before);
    let na = g.context.min(room_after);
    let take = |range: std::ops::Range<u32>| {
        range
            .filter_map(|n| Some((n, line(n)?.to_string())))
            .collect()
    };
    ExtraCtx {
        before: take(first - nb..first),
        after: take(last + 1..last + 1 + na),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse_unified;
    use std::collections::{BTreeSet, HashMap};
    use std::path::{Path, PathBuf};

    fn worktree(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("adhdiff-ctx-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `f.txt` of `lines` lines with the `changed` ones upper-cased.
    fn write_file(dir: &Path, lines: u32, changed: &[u32]) {
        let text: String = (1..=lines)
            .map(|i| {
                if changed.contains(&i) {
                    format!("LINE {i}\n")
                } else {
                    format!("line {i}\n")
                }
            })
            .collect();
        std::fs::write(dir.join("f.txt"), text).unwrap();
    }

    /// A diff of `f.txt` (`lines` long on the new side) with one hunk per
    /// `centers` entry: that line changed, three context lines around it.
    fn diff(lines: u32, centers: &[u32]) -> String {
        let mut out = String::from("diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n");
        for &c in centers {
            let (lo, hi) = (c.saturating_sub(3).max(1), (c + 3).min(lines));
            out += &format!("@@ -{lo},{n} +{lo},{n} @@\n", n = hi - lo + 1);
            for i in lo..=hi {
                if i == c {
                    out += &format!("-line {i}\n+LINE {i}\n");
                } else {
                    out += &format!(" line {i}\n");
                }
            }
        }
        out
    }

    fn inner(files: Vec<FileDiff>, workdir: Option<PathBuf>, context: u32) -> Inner {
        Inner {
            map: adhdiff_shared::AdhDiffMap {
                title: String::new(),
                summary: String::new(),
                diff_sha256: String::new(),
                hunks: Vec::new(),
            },
            files,
            comments: HashMap::new(),
            viewed: BTreeSet::new(),
            state_path: PathBuf::from("/tmp/unused.json"),
            diff_sha: String::new(),
            workdir_hint: workdir,
            editor: None,
            body_cache: HashMap::new(),
            context,
        }
    }

    fn extras(g: &Inner) -> Vec<ExtraCtx> {
        let mut cache = FileLines::new();
        let f = &g.files[0];
        f.hunks
            .iter()
            .map(|h| extra_context(g, f, h, &mut cache))
            .collect()
    }

    fn numbers(rows: &[(u32, String)]) -> Vec<u32> {
        rows.iter().map(|r| r.0).collect()
    }

    fn setup(name: &str, lines: u32, centers: &[u32]) -> (Inner, PathBuf) {
        let dir = worktree(name);
        write_file(&dir, lines, centers);
        let files = parse_unified(diff(lines, centers).as_bytes()).unwrap();
        (inner(files, Some(dir.clone()), 10), dir)
    }

    #[test]
    fn middle_hunk_gets_ten_lines_each_side() {
        let (g, dir) = setup("mid", 40, &[20]);
        let e = &extras(&g)[0];
        assert_eq!(numbers(&e.before), (7..=16).collect::<Vec<_>>());
        assert_eq!(numbers(&e.after), (24..=33).collect::<Vec<_>>());
        assert_eq!(e.before[0].1, "line 7");
        assert_eq!(e.after[9].1, "line 33");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hunk_at_file_start_has_nothing_before() {
        let (g, dir) = setup("start", 40, &[2]);
        let e = &extras(&g)[0];
        assert!(e.before.is_empty());
        assert_eq!(numbers(&e.after), (6..=15).collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hunk_at_file_end_has_fewer_after() {
        let (g, dir) = setup("end", 40, &[34]);
        let e = &extras(&g)[0];
        assert_eq!(numbers(&e.before), (21..=30).collect::<Vec<_>>());
        assert_eq!(numbers(&e.after), vec![38, 39, 40]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn edited_file_gives_no_context() {
        let (g, dir) = setup("drift", 40, &[20]);
        std::fs::write(dir.join("f.txt"), "short\n").unwrap();
        assert_eq!(extras(&g)[0], ExtraCtx::default());
        write_file(&dir, 40, &[20, 18]);
        assert_eq!(extras(&g)[0], ExtraCtx::default());
        write_file(&dir, 40, &[20, 22]);
        assert_eq!(extras(&g)[0], ExtraCtx::default());
        write_file(&dir, 40, &[20, 5]);
        assert_eq!(numbers(&extras(&g)[0].before), (7..=16).collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn neighbouring_hunks_split_the_gap() {
        let (g, dir) = setup("gap", 60, &[10, 25]);
        let e = extras(&g);
        assert_eq!(numbers(&e[0].before), (1..=6).collect::<Vec<_>>());
        assert_eq!(numbers(&e[0].after), (14..=17).collect::<Vec<_>>());
        assert_eq!(numbers(&e[1].before), (18..=21).collect::<Vec<_>>());
        assert_eq!(numbers(&e[1].after), (29..=38).collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn touching_hunks_get_nothing_between_them() {
        let (g, dir) = setup("touch", 60, &[10, 17]);
        let e = extras(&g);
        assert!(e[0].after.is_empty() && e[1].before.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deleted_file_has_no_new_side() {
        let dir = worktree("devnull");
        write_file(&dir, 5, &[]);
        let text = "diff --git a/f.txt b/f.txt\ndeleted file mode 100644\n--- a/f.txt\n+++ /dev/null\n@@ -1,3 +0,0 @@\n-line 1\n-line 2\n-line 3\n";
        let files = parse_unified(text.as_bytes()).unwrap();
        assert_eq!(files[0].new_path, "/dev/null");
        let g = inner(files, Some(dir.clone()), 10);
        assert_eq!(extras(&g)[0], ExtraCtx::default());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unbound_worktree_or_zero_context_gives_nothing() {
        let (mut g, dir) = setup("off", 40, &[20]);
        g.context = 0;
        assert_eq!(extras(&g)[0], ExtraCtx::default());
        g.context = 10;
        g.workdir_hint = None;
        assert_eq!(extras(&g)[0], ExtraCtx::default());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn configured_amount_limits_the_rows() {
        let (mut g, dir) = setup("n", 40, &[20]);
        g.context = 2;
        let e = &extras(&g)[0];
        assert_eq!(numbers(&e.before), vec![15, 16]);
        assert_eq!(numbers(&e.after), vec![24, 25]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
