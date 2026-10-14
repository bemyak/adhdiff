//! Validation of an AdhDiffMap against its source diff.
//!
//! The existential risk is mislabeled hunks hiding real bugs at low
//! tiers. `check` catches the mechanical version of that — a map made
//! for another diff, rows no raw hunk owns, raw hunks left uncovered or
//! covered twice, impossible coordinates — and flags soft issues (giant
//! entries, fragmentation, a bloated P0 tier) as warnings.

use std::collections::HashMap;

use crate::diff::{find_raw_for, parse_unified};
use crate::state::hex_sha256;
use adhdiff_shared::{AdhDiffMap, Confidence, Priority};

#[derive(Debug)]
pub struct Report {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
    pub fn print(&self) {
        for e in &self.errors {
            println!("ERROR  {e}");
        }
        for w in &self.warnings {
            println!("warn   {w}");
        }
        if self.ok() {
            println!(
                "check: OK ({} warning{})",
                self.warnings.len(),
                if self.warnings.len() == 1 { "" } else { "s" }
            );
        } else {
            println!(
                "check: FAILED — {} error{}, {} warning{}",
                self.errors.len(),
                if self.errors.len() == 1 { "" } else { "s" },
                self.warnings.len(),
                if self.warnings.len() == 1 { "" } else { "s" }
            );
        }
    }
}

/// The error text when the map's `diff_sha256` is not the digest of
/// `diff`, naming both; `None` when they match.
pub fn sha_mismatch(map: &AdhDiffMap, diff: &[u8]) -> Option<String> {
    let actual = hex_sha256(diff);
    let claimed = map.diff_sha256.trim();
    if claimed.eq_ignore_ascii_case(&actual) {
        return None;
    }
    Some(format!(
        "map was made for a different diff: diff_sha256 is {}, the diff hashes to {actual}",
        if claimed.is_empty() {
            "(empty)"
        } else {
            claimed
        }
    ))
}

/// Mechanical validation. `strict` promotes low-confidence P5+ hunk
/// labels to errors — fits the "paranoid reviewer" workflow.
pub fn check_map(map: &AdhDiffMap, diff: &[u8], strict: bool) -> Report {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    errors.extend(sha_mismatch(map, diff));

    let files = match parse_unified(diff) {
        Ok(f) => f,
        Err(e) => {
            errors.push(format!("diff does not parse: {e:#}"));
            return Report { errors, warnings };
        }
    };

    let describe = |i: usize| {
        let h = &map.hunks[i];
        format!(
            "'{}' ({}, rows {}-{})",
            h.label,
            h.file.display(),
            h.row_start,
            h.row_end
        )
    };

    // Entry-level sanity first: everything below does arithmetic on rows.
    let mut sane = vec![false; map.hunks.len()];
    for (i, h) in map.hunks.iter().enumerate() {
        if h.row_start == 0 || h.row_end < h.row_start {
            errors.push(format!("bad rows: {}", describe(i)));
        } else {
            sane[i] = true;
        }
    }

    // Duplicate identities.
    let mut by_row: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, h) in map.hunks.iter().enumerate() {
        if sane[i] {
            by_row.entry(h.row_start).or_default().push(i);
        }
    }
    let mut dups: Vec<(&u32, &Vec<usize>)> = by_row.iter().filter(|(_, v)| v.len() > 1).collect();
    dups.sort();
    for (row, idxs) in dups {
        let names: Vec<String> = idxs
            .iter()
            .map(|&i| format!("'{}'", map.hunks[i].label))
            .collect();
        errors.push(format!("duplicate row_start {row}: {}", names.join(", ")));
    }

    // Every entry sits inside exactly one raw hunk; group them by it.
    let mut members: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, h) in map.hunks.iter().enumerate() {
        if !sane[i] {
            continue;
        }
        match find_raw_for(&files, h.row_start) {
            Some((_, raw)) if h.row_end <= raw.row_end => {
                members.entry(raw.row_start).or_default().push(i);
            }
            _ => errors.push(format!(
                "entry {} is not inside one raw hunk of the diff (stale map?)",
                describe(i)
            )),
        }
    }

    // Coverage: each raw hunk's rows are tiled exactly by its entries —
    // sorted by row_start, no gaps, no overlaps, union == range. One
    // whole-hunk entry is the degenerate tiling.
    for f in &files {
        let file = f.path();
        for raw in &f.hunks {
            if raw.row_end < raw.row_start {
                continue;
            }
            let mut segs = members.remove(&raw.row_start).unwrap_or_default();
            segs.sort_by_key(|&i| (map.hunks[i].row_start, i));
            if segs.is_empty() {
                errors.push(format!(
                    "uncovered hunk: {file} rows {}-{}",
                    raw.row_start, raw.row_end
                ));
                continue;
            }

            let mut cursor = raw.row_start;
            for &i in &segs {
                let s = &map.hunks[i];
                if s.row_start > cursor {
                    errors.push(format!(
                        "{file} rows {}-{}: gap — next segment starts at {}, expected {cursor}",
                        raw.row_start, raw.row_end, s.row_start
                    ));
                } else if s.row_start < cursor {
                    errors.push(format!(
                        "{file} rows {}-{}: overlap — segment starts at {}, expected {cursor}",
                        raw.row_start, raw.row_end, s.row_start
                    ));
                }
                cursor = cursor.max(s.row_end.saturating_add(1));
            }
            if cursor <= raw.row_end {
                errors.push(format!(
                    "{file} rows {}-{}: gap — coverage ends at row {}, expected {}",
                    raw.row_start,
                    raw.row_end,
                    cursor - 1,
                    raw.row_end
                ));
            }

            // Fragmentation floor: tiny segments inside a large hunk are
            // noise. Raw hunks of 100 rows or fewer are exempt.
            let hunk_rows = raw.row_end - raw.row_start + 1;
            if hunk_rows > 100 {
                for &i in &segs {
                    let s = &map.hunks[i];
                    let n = s.row_end - s.row_start + 1;
                    if n < 3 {
                        warnings.push(format!(
                            "tiny segment: '{}' spans {n} row{} inside {file} rows {}-{} ({hunk_rows} rows) — widen or merge",
                            s.label,
                            if n == 1 { "" } else { "s" },
                            raw.row_start,
                            raw.row_end,
                        ));
                    }
                }
            }
        }
    }

    // Sanity checks per entry.
    for (i, h) in map.hunks.iter().enumerate() {
        if h.priority == Priority::P0 && h.rationale.trim().is_empty() {
            errors.push(format!("P0 entry {} has no rationale", describe(i)));
        }
        if sane[i] && h.row_end - h.row_start + 1 > 150 && !h.mechanical {
            warnings.push(format!(
                "entry '{}' spans {} rows — consider splitting",
                h.label,
                h.row_end - h.row_start + 1
            ));
        }
        if matches!(h.confidence, Confidence::Low)
            && matches!(h.priority, Priority::P5 | Priority::P6 | Priority::P7)
        {
            let msg = format!(
                "low-confidence label at tier P{}: '{}' — paranoid review should promote this",
                h.priority.as_u8(),
                h.label
            );
            if strict {
                errors.push(msg);
            } else {
                warnings.push(msg);
            }
        }
    }

    // Supports edges: refs must resolve to an entry's row_start, and the
    // resulting forest must be acyclic.
    let parent_of = map.parent_refs();
    for (i, h) in map.hunks.iter().enumerate() {
        if let Some(r) = h.supports
            && parent_of[i].is_none()
        {
            errors.push(format!(
                "entry {} has dangling supports {r} — no entry starts at that row",
                describe(i)
            ));
        }
    }

    // Cycle detection over the parent links (each entry has at most one
    // parent, so chains never branch): state 0 = unseen, 1 = on the
    // current walk, 2 = fully explored.
    let mut state = vec![0u8; map.hunks.len()];
    for start in 0..map.hunks.len() {
        if state[start] == 2 {
            continue;
        }
        let mut path: Vec<usize> = Vec::new();
        let mut cur = start;
        loop {
            match state[cur] {
                2 => break,
                1 => {
                    let pos = path.iter().position(|&i| i == cur).unwrap_or(0);
                    let cyc: Vec<String> = path[pos..]
                        .iter()
                        .chain(std::iter::once(&cur))
                        .map(|&i| {
                            format!("'{}' (row {})", map.hunks[i].label, map.hunks[i].row_start)
                        })
                        .collect();
                    errors.push(format!("supports cycle: {}", cyc.join(" -> ")));
                    break;
                }
                _ => {
                    state[cur] = 1;
                    path.push(cur);
                    match parent_of[cur] {
                        Some(p) => cur = p,
                        None => break,
                    }
                }
            }
        }
        for &i in &path {
            state[i] = 2;
        }
    }

    // Whole-map sanity: P0 should be small — but a pure relocation/refactor
    // diff may legitimately have none.
    let p0_count = map
        .hunks
        .iter()
        .filter(|h| h.priority == Priority::P0)
        .count();
    if p0_count > map.hunks.len() / 4 && map.hunks.len() > 4 {
        warnings.push(format!(
            "P0 count ({p0_count}) is more than a quarter of all entries — gist is probably too broad"
        ));
    }

    Report { errors, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adhdiff_shared::LabeledHunk;
    use std::path::PathBuf;

    /// Two files, two hunks each: a.rs rows 5-7 and 9-11, b.rs rows
    /// 16-18 and 20-22.
    const DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -9,2 +10,3 @@
 ctx
+add-a1
 ctx
@@ -39,2 +40,3 @@
 ctx
+add-a2
 ctx
diff --git a/src/b.rs b/src/b.rs
--- a/src/b.rs
+++ b/src/b.rs
@@ -4,2 +5,3 @@
 ctx
+add-b1
 ctx
@@ -29,2 +30,3 @@
 ctx
+add-b2
 ctx
";

    fn seg(p: Priority, file: &str, start: u32, end: u32, supports: Option<u32>) -> LabeledHunk {
        LabeledHunk {
            file: PathBuf::from(file),
            row_start: start,
            row_end: end,
            priority: p,
            label: format!("{file}:{start}-{end}"),
            rationale: if p == Priority::P0 {
                "the gist".into()
            } else {
                String::new()
            },
            mechanical: false,
            confidence: Default::default(),
            supports,
        }
    }

    fn map_for(diff: &str, hunks: Vec<LabeledHunk>) -> AdhDiffMap {
        AdhDiffMap {
            title: "t".into(),
            summary: "s".into(),
            diff_sha256: hex_sha256(diff.as_bytes()),
            hunks,
        }
    }

    fn check(diff: &str, hunks: Vec<LabeledHunk>) -> Report {
        check_map(&map_for(diff, hunks), diff.as_bytes(), false)
    }

    fn a1(p: Priority, supports: Option<u32>) -> LabeledHunk {
        seg(p, "src/a.rs", 5, 7, supports)
    }
    fn a2(p: Priority, supports: Option<u32>) -> LabeledHunk {
        seg(p, "src/a.rs", 9, 11, supports)
    }
    fn b1(p: Priority, supports: Option<u32>) -> LabeledHunk {
        seg(p, "src/b.rs", 16, 18, supports)
    }
    fn b2(p: Priority, supports: Option<u32>) -> LabeledHunk {
        seg(p, "src/b.rs", 20, 22, supports)
    }

    #[test]
    fn valid_map_two_roots_two_children_passes() {
        let r = check(
            DIFF,
            vec![
                a1(Priority::P0, None),
                a2(Priority::P1, Some(5)),
                b1(Priority::P2, None),
                b2(Priority::P3, Some(16)),
            ],
        );
        assert!(r.errors.is_empty(), "unexpected errors: {:?}", r.errors);
    }

    #[test]
    fn zero_p0_all_p6_passes() {
        let r = check(
            DIFF,
            vec![
                a1(Priority::P6, None),
                a2(Priority::P6, None),
                b1(Priority::P6, None),
                b2(Priority::P6, None),
            ],
        );
        assert!(r.errors.is_empty(), "unexpected errors: {:?}", r.errors);
        assert!(
            r.warnings.is_empty(),
            "unexpected warnings: {:?}",
            r.warnings
        );
    }

    #[test]
    fn dangling_supports_errors() {
        let r = check(
            DIFF,
            vec![
                a1(Priority::P0, None),
                a2(Priority::P1, Some(999)),
                b1(Priority::P2, None),
                b2(Priority::P3, Some(16)),
            ],
        );
        assert!(
            r.errors
                .iter()
                .any(|e| e.contains("dangling supports 999") && e.contains("rows 9-11")),
            "expected dangling-ref error, got: {:?}",
            r.errors
        );
    }

    #[test]
    fn supports_cycle_errors_with_path() {
        let r = check(
            DIFF,
            vec![
                a1(Priority::P0, None),
                b1(Priority::P2, None),
                a2(Priority::P1, Some(20)),
                b2(Priority::P3, Some(9)),
            ],
        );
        assert!(
            r.errors.iter().any(|e| e.starts_with("supports cycle:")
                && e.contains("row 9")
                && e.contains("row 20")),
            "expected cycle error, got: {:?}",
            r.errors
        );
    }

    /// New file `src/gen.rs`, one raw hunk adding `n` lines: the `@@`
    /// is row 4, the body is rows 5..=n+4.
    fn gen_diff(n: u32) -> String {
        let body: String = (1..=n).map(|i| format!("+generated line {i}\n")).collect();
        format!(
            "diff --git a/src/gen.rs b/src/gen.rs\n--- /dev/null\n+++ b/src/gen.rs\n@@ -0,0 +1,{n} @@\n{body}"
        )
    }

    /// Entry over body lines `a..=b` of `gen_diff`.
    fn gen_seg(p: Priority, a: u32, b: u32) -> LabeledHunk {
        seg(p, "src/gen.rs", a + 4, b + 4, None)
    }

    /// One raw hunk deleting 3 lines from `src/gone.rs` (rows 5-7).
    const DEL_DIFF: &str = "\
diff --git a/src/gone.rs b/src/gone.rs
--- a/src/gone.rs
+++ b/src/gone.rs
@@ -10,3 +9,0 @@
-a
-b
-c
";

    #[test]
    fn whole_hunk_entries_pass() {
        let r = check(
            DIFF,
            vec![
                a1(Priority::P0, None),
                a2(Priority::P2, None),
                b1(Priority::P2, None),
                b2(Priority::P2, None),
            ],
        );
        assert!(r.errors.is_empty(), "unexpected errors: {:?}", r.errors);
        assert!(
            r.warnings.is_empty(),
            "unexpected warnings: {:?}",
            r.warnings
        );
    }

    #[test]
    fn three_segment_tiling_passes() {
        let r = check(
            &gen_diff(60),
            vec![
                gen_seg(Priority::P5, 1, 20),
                gen_seg(Priority::P1, 21, 40),
                gen_seg(Priority::P5, 41, 60),
            ],
        );
        assert!(r.errors.is_empty(), "unexpected errors: {:?}", r.errors);
        assert!(
            r.warnings.is_empty(),
            "unexpected warnings: {:?}",
            r.warnings
        );
    }

    #[test]
    fn gap_between_segments_errors_with_expected_start() {
        let r = check(
            &gen_diff(60),
            vec![gen_seg(Priority::P5, 1, 40), gen_seg(Priority::P5, 43, 60)],
        );
        assert!(
            r.errors.iter().any(|e| e.contains("gap")
                && e.contains("src/gen.rs rows 5-64")
                && e.contains("starts at 47, expected 45")),
            "expected gap error naming the expected start, got: {:?}",
            r.errors
        );
    }

    #[test]
    fn gap_at_the_end_errors() {
        let r = check(&gen_diff(60), vec![gen_seg(Priority::P5, 1, 40)]);
        assert!(
            r.errors
                .iter()
                .any(|e| e.contains("coverage ends at row 44, expected 64")),
            "got: {:?}",
            r.errors
        );
    }

    #[test]
    fn overlapping_segments_error() {
        let r = check(
            &gen_diff(60),
            vec![gen_seg(Priority::P5, 1, 40), gen_seg(Priority::P5, 40, 60)],
        );
        assert!(
            r.errors.iter().any(|e| e.contains("overlap")
                && e.contains("src/gen.rs rows 5-64")
                && e.contains("starts at 44, expected 45")),
            "expected overlap error, got: {:?}",
            r.errors
        );
    }

    #[test]
    fn uncovered_raw_hunk_errors() {
        let r = check(
            DIFF,
            vec![
                a1(Priority::P0, None),
                a2(Priority::P2, None),
                b1(Priority::P2, None),
            ],
        );
        assert!(
            r.errors
                .iter()
                .any(|e| e.contains("uncovered hunk: src/b.rs rows 20-22")),
            "got: {:?}",
            r.errors
        );
    }

    #[test]
    fn entry_crossing_a_hunk_boundary_errors() {
        // Rows 45-70 start inside the hunk (rows 5-64) and run past it.
        let r = check(
            &gen_diff(60),
            vec![
                gen_seg(Priority::P5, 1, 40),
                seg(Priority::P5, "src/gen.rs", 45, 70, None),
            ],
        );
        assert!(
            r.errors
                .iter()
                .any(|e| e.contains("not inside one raw hunk") && e.contains("rows 45-70")),
            "got: {:?}",
            r.errors
        );
    }

    #[test]
    fn entry_spanning_the_at_at_row_errors() {
        // Rows 5-9 run from a1's body over the next `@@` row.
        let r = check(
            DIFF,
            vec![
                seg(Priority::P2, "src/a.rs", 5, 9, None),
                a2(Priority::P2, None),
                b1(Priority::P2, None),
                b2(Priority::P2, None),
            ],
        );
        assert!(
            r.errors
                .iter()
                .any(|e| e.contains("not inside one raw hunk")),
            "got: {:?}",
            r.errors
        );
    }

    #[test]
    fn pure_deletion_hunk_is_covered_by_its_rows() {
        let r = check(DEL_DIFF, vec![seg(Priority::P5, "src/gone.rs", 5, 7, None)]);
        assert!(r.errors.is_empty(), "unexpected errors: {:?}", r.errors);
        assert!(
            r.warnings.is_empty(),
            "unexpected warnings: {:?}",
            r.warnings
        );
        let r = check(DEL_DIFF, vec![]);
        assert!(r.errors.iter().any(|e| e.contains("uncovered hunk")));
    }

    #[test]
    fn stale_entry_is_an_error() {
        let r = check(
            &gen_diff(60),
            vec![
                gen_seg(Priority::P5, 1, 60),
                seg(Priority::P5, "src/gen.rs", 500, 502, None),
            ],
        );
        assert!(
            r.errors
                .iter()
                .any(|e| e.contains("not inside one raw hunk") && e.contains("rows 500-502")),
            "got: {:?}",
            r.errors
        );
    }

    #[test]
    fn bad_rows_error_without_panicking() {
        let r = check(
            &gen_diff(60),
            vec![
                seg(Priority::P5, "src/gen.rs", 0, 10, None),
                seg(Priority::P5, "src/gen.rs", 30, 20, None),
                seg(Priority::P5, "src/gen.rs", u32::MAX, u32::MAX, None),
                gen_seg(Priority::P5, 1, 60),
            ],
        );
        assert_eq!(
            r.errors
                .iter()
                .filter(|e| e.starts_with("bad rows"))
                .count(),
            2
        );
        assert!(
            r.errors
                .iter()
                .any(|e| e.contains("not inside one raw hunk"))
        );
    }

    #[test]
    fn duplicate_row_start_errors() {
        let r = check(
            &gen_diff(60),
            vec![gen_seg(Priority::P5, 1, 30), gen_seg(Priority::P5, 1, 60)],
        );
        assert!(
            r.errors.iter().any(|e| e.contains("duplicate row_start 5")),
            "got: {:?}",
            r.errors
        );
    }

    #[test]
    fn sha_mismatch_is_an_error_naming_both_digests() {
        let mut m = map_for(&gen_diff(60), vec![gen_seg(Priority::P5, 1, 60)]);
        m.diff_sha256 = "0".repeat(64);
        let r = check_map(&m, gen_diff(60).as_bytes(), false);
        let actual = hex_sha256(gen_diff(60).as_bytes());
        assert!(
            r.errors
                .iter()
                .any(|e| e.contains(&"0".repeat(64)) && e.contains(&actual)),
            "got: {:?}",
            r.errors
        );
        m.diff_sha256.clear();
        assert!(
            check_map(&m, gen_diff(60).as_bytes(), false)
                .errors
                .iter()
                .any(|e| e.contains("(empty)"))
        );
    }

    #[test]
    fn non_empty_input_without_files_is_an_error() {
        let diff = "this is not a diff\n";
        let r = check(diff, vec![]);
        assert!(!r.ok());
        assert!(
            r.errors.iter().any(|e| e.contains("diff does not parse")),
            "got: {:?}",
            r.errors
        );
    }

    #[test]
    fn p0_without_rationale_is_an_error() {
        let mut h = gen_seg(Priority::P0, 1, 60);
        h.rationale = "  ".into();
        let r = check(&gen_diff(60), vec![h]);
        assert!(
            r.errors
                .iter()
                .any(|e| e.contains("P0 entry") && e.contains("no rationale")),
            "got: {:?}",
            r.errors
        );
    }

    #[test]
    fn dashed_body_lines_do_not_break_coverage() {
        let diff = "diff --git a/q.sql b/q.sql\n--- a/q.sql\n+++ b/q.sql\n@@ -1,2 +1,2 @@\n-- x\n++ y\n keep\n";
        let r = check(diff, vec![seg(Priority::P4, "q.sql", 5, 7, None)]);
        assert!(r.errors.is_empty(), "unexpected errors: {:?}", r.errors);
    }

    #[test]
    fn tiny_segment_warns_only_inside_large_hunks() {
        // 101-row raw hunk: the 2-row trailing segment is tiny.
        let r = check(
            &gen_diff(101),
            vec![
                gen_seg(Priority::P5, 1, 99),
                gen_seg(Priority::P5, 100, 101),
            ],
        );
        assert!(r.errors.is_empty(), "unexpected errors: {:?}", r.errors);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("tiny segment") && w.contains("104-105")),
            "expected tiny-segment warning, got: {:?}",
            r.warnings
        );

        // Same 2-row segment inside a 100-row raw hunk: exempt.
        let r = check(
            &gen_diff(100),
            vec![gen_seg(Priority::P5, 1, 98), gen_seg(Priority::P5, 99, 100)],
        );
        assert!(r.errors.is_empty(), "unexpected errors: {:?}", r.errors);
        assert!(
            !r.warnings.iter().any(|w| w.contains("tiny segment")),
            "unexpected tiny-segment warning: {:?}",
            r.warnings
        );
    }

    #[test]
    fn long_entry_warns_unless_mechanical() {
        let mut h = gen_seg(Priority::P5, 1, 200);
        let r = check(&gen_diff(200), vec![h.clone()]);
        assert!(
            r.warnings.iter().any(|w| w.contains("spans 200 rows")),
            "got: {:?}",
            r.warnings
        );
        h.mechanical = true;
        let r = check(&gen_diff(200), vec![h]);
        assert!(
            !r.warnings.iter().any(|w| w.contains("spans")),
            "got: {:?}",
            r.warnings
        );
    }

    #[test]
    fn low_confidence_low_tier_warns_and_errors_under_strict() {
        let mut h = gen_seg(Priority::P6, 1, 60);
        h.confidence = Confidence::Low;
        let m = map_for(&gen_diff(60), vec![h]);
        let r = check_map(&m, gen_diff(60).as_bytes(), false);
        assert!(r.ok() && r.warnings.iter().any(|w| w.contains("low-confidence")));
        let r = check_map(&m, gen_diff(60).as_bytes(), true);
        assert!(r.errors.iter().any(|e| e.contains("low-confidence")));
    }

    #[test]
    fn priority_derived_picks_min_tier_and_none_on_empty() {
        let a = seg(Priority::P3, "src/x.rs", 1, 2, None);
        let b = seg(Priority::P0, "src/x.rs", 3, 4, None);
        let c = seg(Priority::P5, "src/x.rs", 5, 6, None);
        assert_eq!(Priority::derived(&[&a, &b, &c]), Some(Priority::P0));
        let empty: [&LabeledHunk; 0] = [];
        assert_eq!(Priority::derived(&empty), None);
    }

    #[test]
    fn segments_in_returns_sorted_contained_entries_only() {
        let m = map_for(
            "",
            vec![
                seg(Priority::P1, "src/gen.rs", 11, 20, None),
                seg(Priority::P5, "src/gen.rs", 1, 10, None),
                seg(Priority::P5, "src/gen.rs", 30, 40, None),
                seg(Priority::P5, "src/gen.rs", 15, 25, None),
            ],
        );
        let spans: Vec<(u32, u32)> = m
            .segments_in(1, 20)
            .iter()
            .map(|h| (h.row_start, h.row_end))
            .collect();
        assert_eq!(spans, vec![(1, 10), (11, 20)]);
    }
}
