//! Pure HTML rendering of raw diff-hunk bodies for the Tauri command
//! layer: returns plain `String`s of the exact line-div markup the
//! frontend CSS targets, syntax-highlighted via `highlight`.

use crate::diff::{RawHunk, find_raw_for};
use crate::highlight::{Highlighter, Span};
use crate::state::{Inner, row_pins_for};

/// The body of the raw hunk containing diff row `row`, rendered.
pub fn render_hunk_body_html(g: &Inner, row: u32) -> String {
    match find_raw_for(&g.files, row) {
        Some((f, raw)) => render_raw_hunk_html(g, f.path(), raw),
        None => "(hunk body unavailable)".into(),
    }
}

/// Render one raw hunk's body as `line-*` divs with the shared line-number
/// gutter, comment pins, and tree-sitter highlighting. Every row carries its
/// absolute diff row as `data-row`.
///
/// Sub-hunk segments (the map entries tiling the raw hunk's rows) are
/// baked into row markup: every row belonging to segment `i` carries
/// `seg-p{tier}` + `data-seg="i"`, and each segment's first row also
/// carries `seg-start`. Rows no segment covers (stale maps) get neither.
pub fn render_raw_hunk_html(g: &Inner, file_key: &str, raw: &RawHunk) -> String {
    let hl = Highlighter::for_hunk(file_key, &raw.body);
    let ranges = row_pins_for(g, file_key);
    // Start-sorted tiling of this raw hunk's rows.
    let segs = g.map.segments_in(raw.row_start, raw.row_end);
    let mut buf = String::from(r#"<pre class="hunk-body">"#);
    let mut new_line_no = raw.new_start;
    let mut old_line_no = if raw.old_start > 0 { raw.old_start } else { 0 };
    // Segment cursor: `next_seg` is the first segment not yet passed.
    // Rows are monotone, so a single forward walk resolves every row.
    let mut next_seg = 0usize;
    // Diff-local row ordinal: counts every body row (add/ctx/del/warn) from
    // 0 regardless of side; `data-row` is the same row in diff coordinates.
    for (dline, l) in raw.body.lines().enumerate() {
        let row = raw
            .row_start
            .saturating_add(u32::try_from(dline).unwrap_or(u32::MAX));
        let (cls, prefix, line_attr) = if let Some(rest) = l.strip_prefix('+') {
            let ln = new_line_no;
            new_line_no = new_line_no.saturating_add(1);
            ("line-add", rest, format!(r#"data-oline="{}""#, ln))
        } else if let Some(rest) = l.strip_prefix('-') {
            let ln = old_line_no;
            old_line_no = old_line_no.saturating_add(1);
            ("line-del", rest, format!(r#"data-oold="{}""#, ln))
        } else if l.starts_with('\\') {
            ("line-warn", l, String::new())
        } else {
            let rest = l.strip_prefix(' ').unwrap_or(l);
            let ln = new_line_no;
            new_line_no = new_line_no.saturating_add(1);
            old_line_no = old_line_no.saturating_add(1);
            (
                "line-ctx",
                rest,
                format!(
                    r#"data-oline="{}" data-oold="{}""#,
                    ln,
                    old_line_no.saturating_sub(1)
                ),
            )
        };

        while next_seg < segs.len() && row > segs[next_seg].row_end {
            next_seg += 1;
        }
        let cur_seg = segs
            .get(next_seg)
            .filter(|s| row >= s.row_start)
            .map(|s| (next_seg, *s));

        let commented = ranges.iter().any(|&(s, e)| row >= s && row <= e);

        let mut seg_cls = String::new();
        let mut seg_attr = String::new();
        if let Some((i, s)) = cur_seg {
            seg_cls = format!(" seg-p{}", s.priority.as_u8());
            seg_attr = format!(r#" data-seg="{}""#, i);
            if row == s.row_start {
                seg_cls.push_str(" seg-start");
            }
        }

        let inner_html = spans_html(prefix, hl.spans(dline));
        buf.push_str(&format!(
            r#"<div class="{}{}{}" data-dline="{}" data-row="{}" {}{}><span class="rt">{}</span></div>"#,
            cls,
            if commented { " line-commented" } else { "" },
            seg_cls,
            dline,
            row,
            line_attr,
            seg_attr,
            inner_html
        ));
    }
    buf.push_str("</pre>");
    buf
}

fn spans_html(text: &str, spans: &[Span]) -> String {
    let mut out = String::new();
    let mut at = 0;
    for &(s, e, class) in spans {
        let (Some(gap), Some(body)) = (text.get(at..s), text.get(s..e)) else {
            continue;
        };
        out.push_str(&html_escape(gap));
        out.push_str(&format!(
            r#"<span class="hl-{}">{}</span>"#,
            class.name(),
            html_escape(body)
        ));
        at = e;
    }
    out.push_str(&html_escape(text.get(at..).unwrap_or("")));
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use adhdiff_shared::SavedComment;
    use std::collections::{BTreeSet, HashMap};
    use std::path::PathBuf;

    fn demo_inner() -> Inner {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/segments");
        let map: adhdiff_shared::AdhDiffMap =
            serde_json::from_slice(&std::fs::read(format!("{root}/demo.map.json")).unwrap())
                .unwrap();
        let diff = std::fs::read(format!("{root}/demo.diff")).unwrap();
        let files = crate::diff::parse_unified(&diff).unwrap();
        Inner {
            map,
            files,
            comments: HashMap::new(),
            viewed: BTreeSet::new(),
            state_path: PathBuf::from("/tmp/unused.json"),
            diff_sha: String::new(),
            workdir_hint: None,
            editor: None,
            body_cache: HashMap::new(),
        }
    }

    /// The one-line whole row div containing `needle`.
    fn row_for<'h>(html: &'h str, needle: &str) -> &'h str {
        let at = html
            .find(needle)
            .unwrap_or_else(|| panic!("no row with {needle}"));
        let start = html[..at].rfind("<div").unwrap();
        let end = html[at..].find("</div>").unwrap() + at + "</div>".len();
        &html[start..end]
    }

    #[test]
    fn segment_classes_bake_into_demo_rows() {
        let g = demo_inner();
        let (_, raw) = find_raw_for(&g.files, 6).unwrap();
        let html = render_raw_hunk_html(&g, "src/big.rs", raw);

        // First row of the hunk opens segment 0 (P5, rows 6-145).
        let first = row_for(&html, r#"data-row="6""#);
        assert!(first.contains("seg-p5"), "{first}");
        assert!(first.contains(r#"data-seg="0""#), "{first}");
        assert!(first.contains("seg-start"), "{first}");
        assert!(first.contains(r#"data-oline="1""#), "{first}");

        // Row 145 is still segment 0, mid-segment: no seg-start.
        let last0 = row_for(&html, r#"data-row="145""#);
        assert!(last0.contains("seg-p5"), "{last0}");
        assert!(last0.contains(r#"data-seg="0""#), "{last0}");
        assert!(!last0.contains("seg-start"), "{last0}");

        // Row 146 opens segment 1 (P1): the hand-written lookup core.
        let row146 = row_for(&html, r#"data-row="146""#);
        assert!(row146.contains("seg-p1"), "{row146}");
        assert!(row146.contains(r#"data-seg="1""#), "{row146}");
        assert!(row146.contains("seg-start"), "{row146}");

        // Row 163 ends segment 1 without re-opening it.
        let row163 = row_for(&html, r#"data-row="163""#);
        assert!(row163.contains("seg-p1"), "{row163}");
        assert!(!row163.contains("seg-start"), "{row163}");

        // Row 164 opens segment 2 (P5, tail).
        let row164 = row_for(&html, r#"data-row="164""#);
        assert!(row164.contains("seg-p5"), "{row164}");
        assert!(row164.contains(r#"data-seg="2""#), "{row164}");
        assert!(row164.contains("seg-start"), "{row164}");

        // Exactly one seg-start row per segment.
        assert_eq!(html.matches("seg-start").count(), 3);
    }

    #[test]
    fn rows_outside_any_segment_get_no_class() {
        // No map entries: no segment covers the rows, so they carry
        // neither seg class nor attr.
        let mut g = demo_inner();
        g.map.hunks.clear();
        let raw = RawHunk {
            old_start: 7,
            old_end: 9,
            new_start: 6,
            new_end: 5,
            row_start: 20,
            row_end: 21,
            body: "-gone one\n-gone two\n".into(),
            header: "@@ -7,3 +6,0 @@".into(),
        };
        let html = render_raw_hunk_html(&g, "gone.rs", &raw);
        assert!(html.contains("line-del"));
        assert!(html.contains(r#"data-row="20""#) && html.contains(r#"data-row="21""#));
        assert!(!html.contains("seg-p"), "{html}");
        assert!(!html.contains("data-seg"), "{html}");
    }

    #[test]
    fn comment_pins_mark_any_row_including_deletions() {
        let mut g = demo_inner();
        // Row 193 is lib.rs's removed `let (k, v) = ...unwrap();` line.
        g.comments.insert(
            "src/lib.rs:193".into(),
            vec![SavedComment {
                id: "c".into(),
                file: "src/lib.rs".into(),
                row: 193,
                end_row: Some(194),
                label_at_time: String::new(),
                author: "human".into(),
                body: "b".into(),
                ts_unix: 0,
            }],
        );
        let html = render_hunk_body_html(&g, 190);
        let del = row_for(&html, r#"data-row="193""#);
        assert!(
            del.contains("line-del") && del.contains("line-commented"),
            "{del}"
        );
        assert!(row_for(&html, r#"data-row="194""#).contains("line-commented"));
        assert!(!row_for(&html, r#"data-row="195""#).contains("line-commented"));
        assert!(!row_for(&html, r#"data-row="192""#).contains("line-commented"));
    }

    #[test]
    fn hostile_body_lines_are_escaped() {
        let g = demo_inner();
        let body = "+<script>alert(1)</script>\n+<img src=x onerror=alert(1)>\n+a & \"b\" 'c'\n";
        for file in ["x.txt", "x.html", "x.rs"] {
            let raw = RawHunk {
                old_start: 0,
                old_end: 0,
                new_start: 1,
                new_end: 3,
                row_start: 20,
                row_end: 22,
                body: body.into(),
                header: "@@ -0,0 +1,3 @@".into(),
            };
            let html = render_raw_hunk_html(&g, file, &raw);
            for bad in ["<script", "<img"] {
                assert!(!html.contains(bad), "{file}: {bad} unescaped in {html}");
            }
            assert_eq!(
                html.matches("onerror=").count(),
                html.matches("&lt;img src=x onerror=").count(),
                "{file}: {html}"
            );
            assert!(html.contains("&lt;"), "{file}: {html}");
        }
    }

    #[test]
    fn multibyte_row_html_keeps_text_intact() {
        let g = demo_inner();
        let raw = RawHunk {
            old_start: 0,
            old_end: 0,
            new_start: 1,
            new_end: 1,
            row_start: 20,
            row_end: 20,
            body: "+let s = \"héllo → ✓\";\n".into(),
            header: "@@ -0,0 +1 @@".into(),
        };
        let html = render_raw_hunk_html(&g, "a.rs", &raw);
        assert!(html.contains("&quot;héllo → ✓&quot;"), "{html}");
        assert!(html.contains("hl-string"), "{html}");
    }

    #[test]
    fn every_row_wraps_content_in_rt_span() {
        // The .rt wrapper shrink-wraps the row's selectable box to its
        // glyphs, so a drag selection never paints the gutter or trailing
        // emptiness. Every generated body row must carry exactly one.
        let g = demo_inner();
        let (_, raw) = find_raw_for(&g.files, 6).unwrap();
        let html = render_raw_hunk_html(&g, "src/big.rs", raw);
        let rows = html.matches(r#"data-dline=""#).count();
        assert!(rows > 0);
        assert_eq!(html.matches(r#"<span class="rt">"#).count(), rows);
        assert_eq!(html.matches("</span></div>").count(), rows);
        assert_eq!(html.matches(r#"data-row=""#).count(), rows);
    }
}
