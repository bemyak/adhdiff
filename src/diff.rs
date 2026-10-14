//! Unified diff parser. Reads a standard `git diff` / `jj diff` stream
//! and produces a structured hunk list with file names, old/new ranges,
//! diff rows, and the raw hunk text for agent context.
//!
//! A diff row is the 1-based line number in the raw diff text — the
//! coordinate system every map entry and review anchor uses.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDiff {
    pub old_path: String,
    pub new_path: String,
    pub hunks: Vec<RawHunk>,
    /// The "index" line from git, if present — carries blob hashes.
    pub index: Option<String>,
    /// True if this file was renamed (old_path != new_path).
    pub renamed: bool,
}

impl FileDiff {
    /// The map-facing file name: old path for `/dev/null`-sided files,
    /// new path otherwise.
    pub fn path(&self) -> &str {
        if self.new_path == "/dev/null" {
            &self.old_path
        } else {
            &self.new_path
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawHunk {
    /// Old file range (1-indexed, inclusive).
    pub old_start: u32,
    pub old_end: u32,
    /// New file range (1-indexed, inclusive).
    pub new_start: u32,
    pub new_end: u32,
    /// First body row: the diff row right after the `@@` line (1-indexed).
    pub row_start: u32,
    /// Last body row (inclusive), `\ No newline` rows included. One less
    /// than `row_start` when the hunk has no body rows.
    pub row_end: u32,
    /// The hunk body — lines starting with ' ', '+', '-', or '\\'.
    pub body: String,
    /// The @@ ... @@ header line, verbatim.
    pub header: String,
}

/// Parse a unified-diff byte stream. Returns one FileDiff per changed file.
/// Non-empty input that yields no file is an error.
pub fn parse_unified(input: &[u8]) -> Result<Vec<FileDiff>> {
    let text = String::from_utf8_lossy(input);
    let mut files: Vec<FileDiff> = Vec::new();
    let mut cur_file: Option<FileDiff> = None;
    let mut cur_hunk: Option<RawHunk> = None;
    // Body rows the current `@@` header still promises, per side.
    let (mut old_left, mut new_left) = (0u32, 0u32);
    // Set once a hunk's counts ran out; cleared by the next section header.
    let mut in_tail = false;

    for (i, line) in text.lines().enumerate() {
        let row = u32::try_from(i + 1).context("diff has too many lines")?;

        if let Some(h) = cur_hunk.as_mut() {
            let consumed = match line.as_bytes().first() {
                Some(b'+') if new_left > 0 => {
                    new_left -= 1;
                    true
                }
                Some(b'-') if old_left > 0 => {
                    old_left -= 1;
                    true
                }
                Some(b' ') | None if old_left > 0 && new_left > 0 => {
                    old_left -= 1;
                    new_left -= 1;
                    true
                }
                Some(b'\\') => true,
                _ => false,
            };
            if consumed {
                h.body.push_str(line);
                h.body.push('\n');
                h.row_end = row;
                continue;
            }
            flush_hunk(&mut cur_file, &mut cur_hunk);
            in_tail = true;
        }

        if in_tail {
            if line == "-- " {
                break;
            }
            if matches!(line.as_bytes().first(), Some(b' ' | b'+' | b'-')) {
                bail!("row {row}: hunk body exceeds @@ counts");
            }
        }

        if let Some(rest) = line.strip_prefix("diff --git ") {
            in_tail = false;
            flush_file(&mut files, &mut cur_file, &mut cur_hunk);
            let (old_path, new_path) = split_git_paths(rest);
            cur_file = Some(new_file(old_path, new_path));
        } else if let Some(rest) = line.strip_prefix("diff --cc ") {
            in_tail = false;
            flush_file(&mut files, &mut cur_file, &mut cur_hunk);
            let path = clean_path(rest, "");
            cur_file = Some(new_file(path.clone(), path));
        } else if line.starts_with("@@ ") {
            in_tail = false;
            let (mut h, old_count, new_count) = parse_hunk_header(line)
                .with_context(|| format!("bad hunk header at row {row}: {line}"))?;
            h.row_start = row.checked_add(1).context("diff has too many lines")?;
            h.row_end = row;
            if cur_file.is_some() {
                old_left = old_count;
                new_left = new_count;
                cur_hunk = Some(h);
            }
        } else if let Some(f) = cur_file.as_mut() {
            if let Some(rest) = line.strip_prefix("index ") {
                f.index = Some(rest.to_string());
            } else if let Some(rest) = line
                .strip_prefix("rename from ")
                .or_else(|| line.strip_prefix("copy from "))
            {
                f.old_path = clean_path(rest, "");
            } else if let Some(rest) = line
                .strip_prefix("rename to ")
                .or_else(|| line.strip_prefix("copy to "))
            {
                f.new_path = clean_path(rest, "");
            } else if let Some(rest) = line.strip_prefix("Binary files ") {
                if let Some((old, new)) = rest
                    .strip_suffix(" differ")
                    .and_then(|r| r.split_once(" and "))
                {
                    f.old_path = clean_path(old, "a/");
                    f.new_path = clean_path(new, "b/");
                }
            } else if f.hunks.is_empty() {
                if let Some(rest) = line.strip_prefix("--- ") {
                    f.old_path = clean_path(rest, "a/");
                } else if let Some(rest) = line.strip_prefix("+++ ") {
                    f.new_path = clean_path(rest, "b/");
                }
            }
        }
    }
    flush_file(&mut files, &mut cur_file, &mut cur_hunk);
    if files.is_empty() && !text.trim().is_empty() {
        bail!("no files found in the input — is it a unified diff?");
    }
    Ok(files)
}

fn new_file(old_path: String, new_path: String) -> FileDiff {
    FileDiff {
        old_path,
        new_path,
        hunks: Vec::new(),
        index: None,
        renamed: false,
    }
}

fn flush_hunk(file: &mut Option<FileDiff>, hunk: &mut Option<RawHunk>) {
    if let Some(h) = hunk.take()
        && let Some(f) = file.as_mut()
    {
        f.hunks.push(h);
    }
}

fn flush_file(files: &mut Vec<FileDiff>, file: &mut Option<FileDiff>, hunk: &mut Option<RawHunk>) {
    flush_hunk(file, hunk);
    if let Some(mut f) = file.take() {
        f.renamed = !f.old_path.is_empty()
            && !f.new_path.is_empty()
            && f.old_path != "/dev/null"
            && f.new_path != "/dev/null"
            && f.old_path != f.new_path;
        files.push(f);
    }
}

/// Find the raw hunk whose body rows contain `row`, with its file.
pub fn find_raw_for(files: &[FileDiff], row: u32) -> Option<(&FileDiff, &RawHunk)> {
    files.iter().find_map(|f| {
        f.hunks
            .iter()
            .find(|h| h.row_start <= row && row <= h.row_end)
            .map(|h| (f, h))
    })
}

/// The new-file line an editor should jump to for diff row `row` of `raw`:
/// a `+` or context row maps to its own new-side line; a `-` row to the
/// new-side line of the next `+`/context row in the hunk (the hunk's
/// `new_start` if none); the `@@` row and rows outside the hunk to
/// `new_start`.
pub fn row_to_new_line(raw: &RawHunk, row: u32) -> u32 {
    let Some(idx) = row.checked_sub(raw.row_start) else {
        return raw.new_start;
    };
    let mut next_new = raw.new_start;
    let mut last_new = raw.new_start;
    let mut lines = raw.body.lines().enumerate();
    while let Some((i, line)) = lines.next() {
        let at = u32::try_from(i).unwrap_or(u32::MAX);
        match line.as_bytes().first() {
            Some(b'-') => {
                if at == idx {
                    let has_later = lines.any(|(_, l)| !l.starts_with(['-', '\\']));
                    return if has_later { next_new } else { raw.new_start };
                }
            }
            Some(b'\\') => {
                if at == idx {
                    return last_new;
                }
            }
            _ => {
                if at == idx {
                    return next_new;
                }
                last_new = next_new;
                next_new = next_new.saturating_add(1);
            }
        }
    }
    raw.new_start
}

/// Split the `a/X b/Y` tail of a `diff --git` line into (old, new).
/// Paths with spaces are ambiguous when unquoted: prefer the split whose
/// two halves name the same file, else the first ` b/`.
fn split_git_paths(rest: &str) -> (String, String) {
    if rest.starts_with('"') {
        if let Some(end) = quoted_end(rest) {
            return (
                clean_path(&rest[..=end], "a/"),
                clean_path(rest[end + 1..].trim_start(), "b/"),
            );
        }
    } else if let Some(at) = rest.find(" \"") {
        return (
            clean_path(&rest[..at], "a/"),
            clean_path(&rest[at + 1..], "b/"),
        );
    }
    let candidates: Vec<usize> = rest.match_indices(" b/").map(|(i, _)| i).collect();
    let same = candidates
        .iter()
        .copied()
        .find(|&i| rest[..i].strip_prefix("a/") == Some(&rest[i + 3..]));
    match same.or_else(|| candidates.first().copied()) {
        Some(i) => (
            clean_path(&rest[..i], "a/"),
            clean_path(&rest[i + 1..], "b/"),
        ),
        None => match rest.split_once(' ') {
            Some((old, new)) => (clean_path(old, "a/"), clean_path(new, "b/")),
            None => (clean_path(rest, "a/"), String::new()),
        },
    }
}

/// Byte index of the closing quote of the quoted token starting `s`.
fn quoted_end(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// A path as written in a header: trailing tab dropped, quotes removed
/// and escapes decoded, then the `a/`/`b/` prefix stripped when present.
fn clean_path(raw: &str, prefix: &str) -> String {
    let p = unquote(raw.trim_end_matches('\t'));
    match p.strip_prefix(prefix) {
        Some(stripped) => stripped.to_string(),
        None => p,
    }
}

/// Remove git's C-style quoting: `\t`, `\n`, `\\`, `\"` and `\ooo` octal
/// bytes. Unquoted input is returned unchanged.
fn unquote(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|t| t.strip_suffix('"')) else {
        return s.to_string();
    };
    let b = inner.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 >= b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        match b[i] {
            b't' => out.push(b'\t'),
            b'n' => out.push(b'\n'),
            b'\\' => out.push(b'\\'),
            b'"' => out.push(b'"'),
            b'0'..=b'7' => {
                let mut v = 0u32;
                let mut digits = 0;
                while digits < 3 && i < b.len() && matches!(b[i], b'0'..=b'7') {
                    v = v * 8 + u32::from(b[i] - b'0');
                    i += 1;
                    digits += 1;
                }
                out.push(u8::try_from(v).unwrap_or(b'?'));
                continue;
            }
            other => {
                out.push(b'\\');
                out.push(other);
            }
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse an `@@ -a,b +c,d @@` line into a hunk (rows unset) and its
/// old/new body row counts.
fn parse_hunk_header(line: &str) -> Result<(RawHunk, u32, u32)> {
    let rest = line.strip_prefix("@@ ").context("missing @@ prefix")?;
    let closing = rest.find(" @@").context("missing closing @@")?;
    let inner = &rest[..closing];
    let parts: Vec<&str> = inner.split_whitespace().collect();
    if parts.len() < 2 {
        bail!("hunk header too short");
    }
    let (os, ol) = parse_range(parts[0])?;
    let (ns, nl) = parse_range(parts[1])?;
    let hunk = RawHunk {
        old_start: os,
        old_end: os
            .checked_add(ol.saturating_sub(1))
            .context("old range overflows")?,
        new_start: ns,
        new_end: ns
            .checked_add(nl.saturating_sub(1))
            .context("new range overflows")?,
        row_start: 0,
        row_end: 0,
        body: String::new(),
        header: line.to_string(),
    };
    Ok((hunk, ol, nl))
}

fn parse_range(s: &str) -> Result<(u32, u32)> {
    // "-12,5" or "-12"
    let s = s.trim_start_matches('-').trim_start_matches('+');
    if let Some((a, b)) = s.split_once(',') {
        Ok((a.parse()?, b.parse()?))
    } else {
        Ok((s.parse()?, 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple() {
        let input = b"diff --git a/src/main.rs b/src/main.rs\nindex abc..def 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10,3 +10,4 @@ fn main()\n fn main() {\n-    println!(\"a\");\n+    println!(\"b\");\n+    println!(\"c\");\n }\n";
        let files = parse_unified(input).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].old_path, "src/main.rs");
        assert_eq!(files[0].new_path, "src/main.rs");
        assert_eq!(files[0].hunks.len(), 1);
        let h = &files[0].hunks[0];
        assert_eq!(h.old_start, 10);
        assert_eq!(h.old_end, 12);
        assert_eq!(h.new_start, 10);
        assert_eq!(h.new_end, 13);
        assert_eq!((h.row_start, h.row_end), (6, 10));
    }

    #[test]
    fn parse_multiple_files_and_hunks() {
        let input = b"diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,1 +1,1 @@\n-x\n+y\n@@ -5,1 +5,1 @@\n-p\n+q\ndiff --git a/src/b.rs b/src/b.rs\n--- a/src/b.rs\n+++ b/src/b.rs\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let files = parse_unified(input).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].hunks.len(), 2);
        assert_eq!(files[1].hunks.len(), 1);
        assert!(!files[0].renamed);
        let rows: Vec<(u32, u32)> = files
            .iter()
            .flat_map(|f| f.hunks.iter().map(|h| (h.row_start, h.row_end)))
            .collect();
        assert_eq!(rows, vec![(5, 6), (8, 9), (14, 15)]);
    }

    #[test]
    fn parse_rename() {
        let input = b"diff --git a/old.rs b/new.rs\nsimilarity index 90%\nrename from old.rs\nrename to new.rs\n--- a/old.rs\n+++ b/new.rs\n@@ -1,1 +1,1 @@\n-x\n+y\n";
        let files = parse_unified(input).unwrap();
        assert_eq!(files[0].old_path, "old.rs");
        assert_eq!(files[0].new_path, "new.rs");
        assert!(files[0].renamed);
    }

    #[test]
    fn parse_new_file_is_not_rename() {
        let input = b"diff --git a/dev/null b/NEW.md\nindex 0000000..2781ac2\n--- /dev/null\n+++ b/NEW.md\n@@ -0,0 +1 @@\n+# x\n";
        let files = parse_unified(input).unwrap();
        assert_eq!(files[0].new_path, "NEW.md");
        assert!(!files[0].renamed);
    }

    #[test]
    fn body_lines_that_look_like_headers_stay_in_the_body() {
        let input = b"diff --git a/x.sql b/x.sql\n--- a/x.sql\n+++ b/x.sql\n@@ -1,2 +1,2 @@\n--- old comment\n+++ new\n ctx\n";
        let files = parse_unified(input).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].old_path, "x.sql");
        assert_eq!(files[0].new_path, "x.sql");
        let h = &files[0].hunks[0];
        assert_eq!(h.body, "--- old comment\n+++ new\n ctx\n");
        assert_eq!((h.row_start, h.row_end), (5, 7));
    }

    #[test]
    fn rows_beyond_the_hunk_counts_are_an_error() {
        let head = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-a\n+b\n";
        let err = parse_unified(format!("{head}+c\n+d\n").as_bytes()).unwrap_err();
        assert!(err.to_string().contains("row 7"), "{err}");
        // A side without budget never consumes its rows.
        let input = b"diff --git a/f b/f\n@@ -1,1 +0,0 @@\n-a\n+b\n";
        let err = parse_unified(input).unwrap_err();
        assert!(err.to_string().contains("row 4"), "{err}");
        let input = b"diff --git a/f b/f\n@@ -0,0 +1,1 @@\n+a\n-b\n";
        assert!(parse_unified(input).is_err());
    }

    #[test]
    fn format_patch_signature_ends_parsing() {
        let input =
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-a\n+b\n-- \n2.40.0\n\n";
        let files = parse_unified(input.as_bytes()).unwrap();
        let h = &files[0].hunks[0];
        assert_eq!((h.row_start, h.row_end), (5, 6));
    }

    #[test]
    fn no_newline_marker_is_part_of_the_hunk() {
        let input = b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\ndiff --git a/g b/g\n";
        let files = parse_unified(input).unwrap();
        let h = &files[0].hunks[0];
        assert_eq!((h.row_start, h.row_end), (5, 8));
        assert_eq!(h.body.lines().count(), 4);
        assert_eq!(files[1].old_path, "g");
    }

    #[test]
    fn binary_mode_only_and_pure_rename_keep_their_paths() {
        let input = b"diff --git a/img.png b/img.png\nindex 1..2 100644\nBinary files a/img.png and b/img.png differ\ndiff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\ndiff --git a/old name.rs b/new name.rs\nsimilarity index 100%\nrename from old name.rs\nrename to new name.rs\ndiff --git a/n.bin b/n.bin\nnew file mode 100644\nBinary files /dev/null and b/n.bin differ\n";
        let files = parse_unified(input).unwrap();
        assert_eq!(files.len(), 4);
        assert_eq!(
            (files[0].old_path.as_str(), files[0].new_path.as_str()),
            ("img.png", "img.png")
        );
        assert_eq!(
            (files[1].old_path.as_str(), files[1].new_path.as_str()),
            ("run.sh", "run.sh")
        );
        assert_eq!(
            (files[2].old_path.as_str(), files[2].new_path.as_str()),
            ("old name.rs", "new name.rs")
        );
        assert!(files[2].renamed);
        assert_eq!(files[3].old_path, "/dev/null");
        assert_eq!(files[3].path(), "n.bin");
    }

    #[test]
    fn paths_strip_one_prefix_tabs_and_quotes() {
        let input = b"diff --git a/a/x.rs b/a/x.rs\n--- a/a/x.rs\t\n+++ b/a/x.rs\t\n@@ -1 +1 @@\n-p\n+q\ndiff --git \"a/sp\\303\\251ce\\t.rs\" \"b/sp\\303\\251ce\\t.rs\"\n--- \"a/sp\\303\\251ce\\t.rs\"\n+++ \"b/sp\\303\\251ce\\t.rs\"\n@@ -1 +1 @@\n-p\n+q\n";
        let files = parse_unified(input).unwrap();
        assert_eq!(files[0].old_path, "a/x.rs");
        assert_eq!(files[0].new_path, "a/x.rs");
        assert_eq!(files[1].old_path, "sp\u{e9}ce\t.rs");
        assert_eq!(files[1].new_path, "sp\u{e9}ce\t.rs");
    }

    #[test]
    fn git_header_with_spaces_prefers_the_matching_split() {
        let files = parse_unified(b"diff --git a/x b/y b/x b/y\n").unwrap();
        assert_eq!(files[0].old_path, "x b/y");
        assert_eq!(files[0].new_path, "x b/y");
    }

    #[test]
    fn overflowing_header_is_an_error_not_a_panic() {
        let input = b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -4294967295,5 +1 @@\n-x\n";
        assert!(parse_unified(input).is_err());
        assert!(parse_unified(b"diff --git a/f b/f\n@@ -1 +4294967295,9 @@\n+x\n").is_err());
    }

    #[test]
    fn garbage_is_an_error_and_empty_is_not() {
        assert!(parse_unified(b"garbage\n").is_err());
        assert!(parse_unified(b"").unwrap().is_empty());
        assert!(parse_unified(b"\n  \n").unwrap().is_empty());
    }

    #[test]
    fn invalid_utf8_parses_lossily() {
        let input = b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-\xff\n+ok\n";
        let files = parse_unified(input).unwrap();
        assert!(files[0].hunks[0].body.contains('\u{fffd}'));
    }

    /// f.rs: an edit hunk (rows 5-10) and a pure-deletion hunk (rows
    /// 12-14); g.rs: one 2-row edit hunk (rows 19-20).
    fn rows_fixture() -> Vec<FileDiff> {
        let input = b"diff --git a/f.rs b/f.rs\n--- a/f.rs\n+++ b/f.rs\n@@ -10,2 +10,5 @@\n a\n-b\n+c\n+d\n+e\n+f\n@@ -30,3 +24,0 @@\n-x\n-y\n-z\ndiff --git a/g.rs b/g.rs\n--- a/g.rs\n+++ b/g.rs\n@@ -3,1 +3,1 @@\n-p\n+q\n";
        parse_unified(input).unwrap()
    }

    #[test]
    fn find_raw_for_resolves_rows() {
        let files = rows_fixture();
        let at = |row| find_raw_for(&files, row).map(|(f, h)| (f.path().to_string(), h.row_start));
        assert_eq!(at(5), Some(("f.rs".into(), 5)));
        assert_eq!(at(10), Some(("f.rs".into(), 5)));
        assert_eq!(at(12), Some(("f.rs".into(), 12)));
        assert_eq!(at(14), Some(("f.rs".into(), 12)));
        assert_eq!(at(19), Some(("g.rs".into(), 19)));
        // @@ rows, headers, and rows past the end belong to no hunk.
        for row in [0, 1, 4, 11, 15, 18, 21, 999] {
            assert_eq!(at(row), None, "row {row}");
        }
    }

    #[test]
    fn row_to_new_line_follows_the_editor_jump_rule() {
        let files = rows_fixture();
        let edit = &files[0].hunks[0];
        // @@ row, then context, deletion, additions.
        assert_eq!(row_to_new_line(edit, 4), 10);
        assert_eq!(row_to_new_line(edit, 5), 10);
        assert_eq!(row_to_new_line(edit, 6), 11);
        assert_eq!(row_to_new_line(edit, 7), 11);
        assert_eq!(row_to_new_line(edit, 10), 14);
        // A `-` row with nothing after it falls back to new_start.
        let del = &files[0].hunks[1];
        assert_eq!(row_to_new_line(del, 12), 24);
        assert_eq!(row_to_new_line(del, 14), 24);
        assert_eq!(row_to_new_line(del, 999), 24);
    }

    #[test]
    fn row_to_new_line_maps_deletions_to_the_next_surviving_row() {
        let input = b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,5 +1,2 @@\n-a\n-b\n c\n-d\n e\n\\ No newline at end of file\n";
        let files = parse_unified(input).unwrap();
        let h = &files[0].hunks[0];
        assert_eq!(row_to_new_line(h, 5), 1);
        assert_eq!(row_to_new_line(h, 6), 1);
        assert_eq!(row_to_new_line(h, 7), 1);
        assert_eq!(row_to_new_line(h, 8), 2);
        assert_eq!(row_to_new_line(h, 9), 2);
        assert_eq!(row_to_new_line(h, 10), 2);
    }
}
