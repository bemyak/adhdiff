//! Supports-forest outline helpers + the `MapPayload` wire builders.
//!
//! One outline node per RAW diff hunk, never one per map segment: the
//! entries tiling a raw hunk's rows hang under the anchor entry
//! (`row_start == raw.row_start`), and the raw hunk's `file` + first body
//! row supply the item's identity, viewed key, and comment span.
//! Ordering follows the group-level supports forest — roots by derived
//! priority (strongest segment tier), subtrees depth-first. (The traversal
//! math is duplicated in `tui` — deliberately not unified with that
//! copy.) The payload constructors feed `get_map`.

use std::collections::HashMap;

use adhdiff_shared::{
    AdhDiffMap, FileBodies, HunkBody, HunkPayload, MapPayload, OutlineItem, Priority,
    SegmentPayload,
};

use crate::context::FileLines;
use crate::diff::{FileDiff, find_raw_for};
use crate::hunk_html::render_hunk_body_html;
use crate::state::{Inner, comments_at};

/// Depth of group `gi` under its supports ancestors (0 = root), bounded
/// so a cyclic map (a hard `check` error) can't hang the render.
pub fn outline_depth(parent: &[Option<usize>], gi: usize) -> usize {
    let mut d = 0;
    let mut cur = gi;
    while let Some(p) = parent[cur] {
        if d >= parent.len() {
            break;
        }
        d += 1;
        cur = p;
    }
    d
}

/// Viewed identity of a raw hunk: `file:row_start` — the same string
/// persist_sidecar keeps under "@viewed".
pub fn hunk_identity(file: &str, row: u32) -> String {
    format!("{file}:{row}")
}

/// One raw diff hunk as an outline node: the anchor entry plus every map
/// entry resolving into it.
pub(crate) struct RawGroup {
    /// Map index of the anchor entry — `row_start == raw.row_start`, or
    /// the first member when the map does not tile the hunk from its
    /// first row.
    pub anchor: usize,
    /// All entries resolving into this raw hunk, sorted by
    /// (row_start, map index).
    pub members: Vec<usize>,
    /// Entries contained in the raw span — the payload's `segments`
    /// list, start-sorted like `AdhDiffMap::segments_in` returns them.
    pub segments: Vec<usize>,
    /// The diff's file path for this hunk.
    pub file: String,
    /// The raw hunk's body rows `row_start..=row_end`.
    pub span: (u32, u32),
    /// Derived priority: the strongest tier among the members.
    pub priority: Priority,
    /// Member supplying label/rationale: strongest tier, tie → earliest
    /// start.
    pub label_idx: usize,
    /// Group-level supports parent as an index into the groups vec this
    /// struct ships in; None = root. Edges staying inside one group (a
    /// segment supporting its sibling) don't become outline edges.
    pub parent: Option<usize>,
    /// The external supports edge re-anchored to the target group's
    /// first body row; None for roots.
    pub supports: Option<u32>,
}

impl RawGroup {
    /// Viewed key / hunk identity: `file:row_start` of the raw hunk.
    pub fn identity(&self) -> String {
        hunk_identity(&self.file, self.span.0)
    }
}

/// Group `map` entries by containing raw hunk, then order the groups
/// like `AdhDiffMap::reading_order`: roots by (derived priority, anchor
/// map index), each root followed depth-first by its subtree. Entries
/// resolving to no raw hunk (stale — `check` rejects them) contribute
/// nothing; raw hunks without entries never become nodes.
pub(crate) fn raw_groups(map: &AdhDiffMap, files: &[FileDiff]) -> Vec<RawGroup> {
    // Keyed by the raw hunk's row_start: (file, row_end, member map
    // indices); `keys` keeps first-seen order.
    let mut keys: Vec<u32> = Vec::new();
    let mut by_raw: HashMap<u32, (String, u32, Vec<usize>)> = HashMap::new();
    for (mi, h) in map.hunks.iter().enumerate() {
        let Some((f, raw)) = find_raw_for(files, h.row_start) else {
            continue;
        };
        match by_raw.entry(raw.row_start) {
            std::collections::hash_map::Entry::Occupied(mut e) => e.get_mut().2.push(mi),
            std::collections::hash_map::Entry::Vacant(v) => {
                keys.push(raw.row_start);
                v.insert((f.path().to_string(), raw.row_end, vec![mi]));
            }
        }
    }

    let mut groups: Vec<RawGroup> = Vec::with_capacity(keys.len());
    for key in keys {
        let Some((file, row_end, mut members)) = by_raw.remove(&key) else {
            continue;
        };
        members.sort_by_key(|&mi| (map.hunks[mi].row_start, mi));
        let anchor = members
            .iter()
            .copied()
            .find(|&mi| map.hunks[mi].row_start == key)
            .unwrap_or(members[0]);
        let segments: Vec<usize> = members
            .iter()
            .copied()
            .filter(|&mi| {
                let h = &map.hunks[mi];
                h.row_start <= h.row_end && h.row_end <= row_end
            })
            .collect();
        let priority =
            Priority::derived(&members.iter().map(|&mi| &map.hunks[mi]).collect::<Vec<_>>())
                .unwrap_or(map.hunks[anchor].priority);
        let label_idx = members
            .iter()
            .copied()
            .min_by_key(|&mi| (map.hunks[mi].priority, map.hunks[mi].row_start, mi))
            .unwrap_or(anchor);
        groups.push(RawGroup {
            anchor,
            members,
            segments,
            file,
            span: (key, row_end),
            priority,
            label_idx,
            parent: None,
            supports: None,
        });
    }

    // Lift entry-level supports edges to groups: the first member edge
    // resolving into ANOTHER group is the outline parent; edges internal
    // to one raw hunk stay invisible at hunk level.
    let entry_parent = map.parent_refs();
    let mut group_of: HashMap<usize, usize> = HashMap::with_capacity(map.hunks.len());
    for (gi, gr) in groups.iter().enumerate() {
        for &mi in &gr.members {
            group_of.insert(mi, gi);
        }
    }
    for (gi, gr) in groups.iter_mut().enumerate() {
        gr.parent = gr.members.iter().find_map(|&mi| {
            entry_parent[mi]
                .and_then(|t| group_of.get(&t).copied())
                .filter(|&t| t != gi)
        });
    }
    for gi in 0..groups.len() {
        if let Some(pgi) = groups[gi].parent {
            groups[gi].supports = Some(groups[pgi].span.0);
        }
    }

    // Root-first DFS over the group forest — same shape as
    // AdhDiffMap::reading_order, including the cycle fallback that
    // degrades unreachable nodes to roots appended at the end.
    let n = groups.len();
    let kids = |i: usize| -> Vec<usize> {
        let mut out: Vec<usize> = (0..n).filter(|&k| groups[k].parent == Some(i)).collect();
        out.sort_by_key(|&k| (groups[k].priority, groups[k].anchor));
        out
    };
    let visit = |start: usize, visited: &mut [bool], out: &mut Vec<usize>| {
        let mut stack = vec![start];
        while let Some(i) = stack.pop() {
            if visited[i] {
                continue;
            }
            visited[i] = true;
            out.push(i);
            let mut kids = kids(i);
            kids.reverse(); // LIFO pops them in forward order
            stack.extend(kids);
        }
    };
    let mut visited = vec![false; n];
    let mut order: Vec<usize> = Vec::with_capacity(n);
    let mut roots: Vec<usize> = (0..n).filter(|&i| groups[i].parent.is_none()).collect();
    roots.sort_by_key(|&i| (groups[i].priority, groups[i].anchor));
    for r in roots {
        visit(r, &mut visited, &mut order);
    }
    let mut stuck: Vec<usize> = (0..n).filter(|&i| !visited[i]).collect();
    stuck.sort_by_key(|&i| (groups[i].priority, groups[i].anchor));
    for r in stuck {
        visit(r, &mut visited, &mut order);
    }

    let mut position = vec![0usize; n];
    for (pos, &gi) in order.iter().enumerate() {
        position[gi] = pos;
    }
    let mut drained: Vec<Option<RawGroup>> = groups.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(n);
    for gi in order {
        let mut gr = drained[gi].take().unwrap();
        gr.parent = gr.parent.map(|p| position[p]);
        out.push(gr);
    }
    out
}

/// The outline node list for the current state, in reading order.
fn payload_groups(g: &Inner) -> Vec<RawGroup> {
    raw_groups(&g.map, &g.files)
}

/// One raw hunk as it crosses IPC: identity, labels, row count, and the
/// comments/viewed state attached to its range. Metadata only — the
/// highlighted body markup rides `hunk_body` and is fetched on demand.
/// Segment-level entries surface denormalized in `segments`; everything
/// hunk-level (priority, label, comments span) is derived here.
fn group_payload(g: &Inner, gr: &RawGroup, oi: usize) -> HunkPayload {
    let (ctx_head, ctx_tail) = find_raw_for(&g.files, gr.span.0).map_or((0, 0), |(_, raw)| {
        let is_ctx = |l: &&str| l.is_empty() || l.starts_with(' ');
        let head = raw.body.lines().take_while(is_ctx).count();
        let tail = raw
            .body
            .lines()
            .rev()
            .skip_while(|l| l.starts_with('\\'))
            .take_while(is_ctx)
            .count();
        (head as u32, tail as u32)
    });
    let anchor = &g.map.hunks[gr.anchor];
    let label_h = &g.map.hunks[gr.label_idx];
    HunkPayload {
        idx: oi,
        map_idx: gr.anchor,
        file: gr.file.clone(),
        row_start: gr.span.0,
        row_end: gr.span.1,
        priority: gr.priority,
        label: label_h.label.clone(),
        rationale: label_h.rationale.clone(),
        mechanical: anchor.mechanical,
        confidence: anchor.confidence,
        supports: gr.supports,
        body_rows: gr.span.1.saturating_sub(gr.span.0).saturating_add(1),
        segments: gr
            .segments
            .iter()
            .map(|&mi| {
                let s = &g.map.hunks[mi];
                SegmentPayload {
                    row_start: s.row_start,
                    row_end: s.row_end,
                    tier: s.priority.as_u8(),
                    label: s.label.clone(),
                    rationale: s.rationale.clone(),
                }
            })
            .collect(),
        ctx_head,
        ctx_tail,
        comments: comments_at(g, &gr.file, gr.span.0, gr.span.1),
        viewed: g.viewed.contains(&gr.identity()),
        openable: g.workdir_hint.as_ref().is_some_and(|wd| {
            g.files
                .iter()
                .any(|f| f.path() == gr.file && f.new_path != "/dev/null")
                && crate::editor::openable(&gr.file, wd)
        }),
    }
}

/// Outline (reading-order) position of the raw hunk map index `mi`
/// belongs to — `None` when the entry resolves to no raw hunk at all.
pub fn outline_position(g: &Inner, mi: usize) -> Option<usize> {
    payload_groups(g)
        .iter()
        .position(|gr| gr.members.contains(&mi))
}

/// A raw hunk's rendered body + the comments covering its whole range.
/// First render goes through tree-sitter; the result is cached in
/// `g.body_cache` under the anchor's map index and reused until a
/// comment mutation on the file invalidates it.
/// `mi` may name any entry of the hunk — segment or anchor alike.
pub fn hunk_body(g: &mut Inner, mi: usize, oi: usize) -> Option<HunkBody> {
    hunk_body_in(g, mi, oi, &mut FileLines::new())
}

fn hunk_body_in(g: &mut Inner, mi: usize, oi: usize, files: &mut FileLines) -> Option<HunkBody> {
    let gr = payload_groups(g)
        .into_iter()
        .find(|gr| gr.members.contains(&mi))?;
    let anchor_mi = gr.anchor;
    if !g.body_cache.contains_key(&anchor_mi) {
        let html = render_hunk_body_html(g, gr.span.0, files);
        g.body_cache.insert(anchor_mi, html);
    }
    let body_html = g.body_cache.get(&anchor_mi)?.clone();
    Some(HunkBody {
        idx: oi,
        map_idx: anchor_mi,
        body_html,
        comments: comments_at(g, &gr.file, gr.span.0, gr.span.1),
    })
}

/// Every raw hunk of one file in reading order — the slice a comment
/// mutation repaints.
pub fn file_bodies(g: &mut Inner, file: &str) -> FileBodies {
    let pairs: Vec<(usize, usize)> = payload_groups(g)
        .into_iter()
        .enumerate()
        .filter(|(_, gr)| gr.file == file)
        .map(|(oi, gr)| (gr.anchor, oi))
        .collect();
    let mut files = FileLines::new();
    let bodies = pairs
        .into_iter()
        .filter_map(|(mi, oi)| hunk_body_in(g, mi, oi, &mut files))
        .collect();
    FileBodies {
        file: file.to_string(),
        bodies,
    }
}

/// Drop cached bodies for one file's hunks (a comment mutation changes
/// the pin markup, so every rendered body of that file goes stale).
pub fn invalidate_file_bodies(g: &mut Inner, file: &str) {
    let stale: Vec<usize> = payload_groups(g)
        .iter()
        .filter(|gr| gr.file == file)
        .map(|gr| gr.anchor)
        .collect();
    for mi in stale {
        g.body_cache.remove(&mi);
    }
}

/// Everything the frontend needs to render in one shot: one hunk item
/// per raw hunk, segments denormalized inside it.
pub fn map_payload(g: &Inner) -> MapPayload {
    let groups = payload_groups(g);
    let parent: Vec<Option<usize>> = groups.iter().map(|gr| gr.parent).collect();
    let hunks: Vec<HunkPayload> = groups
        .iter()
        .enumerate()
        .map(|(oi, gr)| group_payload(g, gr, oi))
        .collect();
    let outline: Vec<OutlineItem> = groups
        .iter()
        .enumerate()
        .map(|(oi, gr)| OutlineItem {
            idx: oi,
            depth: outline_depth(&parent, oi),
            label: g.map.hunks[gr.label_idx].label.clone(),
            commented: !comments_at(g, &gr.file, gr.span.0, gr.span.1).is_empty(),
            viewed: g.viewed.contains(&gr.identity()),
        })
        .collect();
    MapPayload {
        title: g.map.title.clone(),
        summary: g.map.summary.clone(),
        diff_sha12: g.diff_sha[..12].to_string(),
        workdir: g.workdir_hint.as_ref().map(|w| w.display().to_string()),
        editor: g
            .editor
            .as_deref()
            .and_then(|e| e.split_whitespace().next())
            .map(str::to_string),
        hunks,
        outline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adhdiff_shared::{LabeledHunk, SavedComment};

    fn demo() -> (AdhDiffMap, Vec<FileDiff>) {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/segments");
        let map: AdhDiffMap =
            serde_json::from_slice(&std::fs::read(format!("{root}/demo.map.json")).unwrap())
                .unwrap();
        let diff = std::fs::read(format!("{root}/demo.diff")).unwrap();
        let files = crate::diff::parse_unified(&diff).unwrap();
        (map, files)
    }

    #[test]
    fn demo_fixture_groups_per_raw_hunk() {
        let (map, files) = demo();
        let groups = raw_groups(&map, &files);
        // Three raw hunks → three outline items, no matter how many
        // segment entries tile them.
        assert_eq!(groups.len(), 3);

        // Reading order follows the derived priority: big.rs (P1), then
        // lib.rs (P2), then the CHANGELOG (P4).
        let big = &groups[0];
        assert_eq!(big.file, "src/big.rs");
        assert_eq!(big.priority, Priority::P1);
        assert_eq!(big.segments.len(), 3);
        assert_eq!(big.span, (6, 185));
        assert_eq!(map.hunks[big.anchor].row_start, 6);
        assert_eq!(big.identity(), "src/big.rs:6");
        assert_eq!(
            map.hunks[big.label_idx].label,
            "hand-written lookup: binary search plus bucket check"
        );
        // Segment-internal supports edges (sibling segments pointing at
        // the lookup core) do not become outline edges.
        assert_eq!(big.parent, None);
        assert_eq!(big.supports, None);
        let seg_starts: Vec<u32> = big
            .segments
            .iter()
            .map(|&mi| map.hunks[mi].row_start)
            .collect();
        assert_eq!(seg_starts, vec![6, 146, 164]);

        let lib = &groups[1];
        assert_eq!(lib.file, "src/lib.rs");
        assert_eq!(lib.priority, Priority::P2);
        assert_eq!(lib.segments.len(), 2);
        assert_eq!(lib.span, (190, 200));
        assert_eq!(map.hunks[lib.anchor].row_start, 190);
        assert_eq!(map.hunks[lib.label_idx].priority, Priority::P2);

        // Degenerate tiling: one entry spanning the whole hunk.
        let changelog = &groups[2];
        assert_eq!(changelog.file, "docs/CHANGELOG.md");
        assert_eq!(changelog.priority, Priority::P4);
        assert_eq!(changelog.segments.len(), 1);
        assert_eq!(changelog.members.len(), 1);
    }

    #[test]
    fn demo_payload_ships_one_hunk_per_raw_hunk_with_nested_segments() {
        let (map, files) = demo();
        let g = Inner {
            map,
            files,
            comments: Default::default(),
            viewed: Default::default(),
            state_path: "/tmp/unused.json".into(),
            diff_sha: "0".repeat(16),
            workdir_hint: None,
            editor: None,
            body_cache: Default::default(),
            context: 0,
        };
        let payload = map_payload(&g);
        // Six labeled entries, three articles: the entries ride inside
        // their raw hunk's `segments`, never as hunks of their own.
        assert_eq!(payload.hunks.len(), 3);
        assert_eq!(payload.outline.len(), 3);
        let shape: Vec<_> = payload
            .hunks
            .iter()
            .map(|h| {
                (
                    h.file.as_str(),
                    h.row_start,
                    h.row_end,
                    h.segments
                        .iter()
                        .map(|s| (s.row_start, s.row_end, s.tier))
                        .collect(),
                )
            })
            .collect();
        let ctx: Vec<(u32, u32)> = payload
            .hunks
            .iter()
            .map(|h| (h.ctx_head, h.ctx_tail))
            .collect();
        assert_eq!(ctx, vec![(0, 0), (3, 3), (2, 1)]);
        assert_eq!(
            shape,
            vec![
                (
                    "src/big.rs",
                    6,
                    185,
                    vec![(6, 145, 5), (146, 163, 1), (164, 185, 5)]
                ),
                ("src/lib.rs", 190, 200, vec![(190, 197, 2), (198, 200, 4)]),
                ("docs/CHANGELOG.md", 205, 209, vec![(205, 209, 4)]),
            ]
        );
    }

    #[test]
    fn stale_entries_drop_out_of_the_outline() {
        let (mut map, files) = demo();
        // An entry resolving to no raw hunk must not produce a node.
        map.hunks[5].row_start = 9000;
        map.hunks[5].row_end = 9001;
        let groups = raw_groups(&map, &files);
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|gr| gr.file != "docs/CHANGELOG.md"));
    }

    #[test]
    fn cross_group_supports_becomes_an_outline_edge() {
        let (mut map, files) = demo();
        // lib.rs's first entry supports the lookup core's segment.
        map.hunks[3].supports = Some(146);
        let groups = raw_groups(&map, &files);
        assert_eq!(groups[0].file, "src/big.rs");
        assert_eq!(groups[1].file, "src/lib.rs");
        assert_eq!(groups[1].parent, Some(0));
        assert_eq!(groups[1].supports, Some(6));
    }

    #[test]
    fn deletion_hunk_group_spans_its_rows() {
        // Pure deletion of old lines 10-12: rows 5-7 of the diff.
        let diff = b"diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -10,5 +9,2 @@ fn\n ctx\n-gone a\n-gone b\n-gone c\n ctx2\n";
        let files = crate::diff::parse_unified(diff).unwrap();
        let entry = LabeledHunk {
            file: "f.txt".into(),
            row_start: 5,
            row_end: 9,
            priority: Priority::P2,
            label: "drop legacy flags".into(),
            rationale: "r".into(),
            mechanical: false,
            confidence: Default::default(),
            supports: None,
        };
        let map = AdhDiffMap {
            title: "t".into(),
            summary: "s".into(),
            hunks: vec![entry],
            diff_sha256: String::new(),
        };

        let groups = raw_groups(&map, &files);
        assert_eq!(groups.len(), 1);
        let gr = &groups[0];
        assert_eq!(gr.span, (5, 9));
        assert_eq!(gr.identity(), "f.txt:5");

        // A comment on a removed row lands in the payload.
        let mut comments = std::collections::HashMap::new();
        comments.insert(
            "f.txt:7".to_string(),
            vec![SavedComment {
                id: "c1".into(),
                file: "f.txt".into(),
                row: 7,
                end_row: None,
                label_at_time: String::new(),
                author: "human".into(),
                body: "note".into(),
                ts_unix: 0,
            }],
        );
        let g = Inner {
            map,
            files,
            comments,
            viewed: Default::default(),
            state_path: "/tmp/unused.json".into(),
            diff_sha: "0".repeat(16),
            workdir_hint: None,
            editor: None,
            body_cache: Default::default(),
            context: 0,
        };
        let payload = map_payload(&g);
        assert_eq!(payload.hunks.len(), 1);
        let item = &payload.hunks[0];
        assert_eq!((item.row_start, item.row_end), (5, 9));
        assert_eq!(item.body_rows, 5);
        assert_eq!(item.comments.len(), 1);
        assert_eq!(item.comments[0].body, "note");
    }
}
