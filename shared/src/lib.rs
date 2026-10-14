//! Data model for adhdiff — the core types a reviewer agent produces
//! and a human consumes, plus the review-state and IPC wire types the
//! Tauri commands exchange with the Leptos frontend.
//!
//! This crate is shared between the backend binary and the WASM frontend,
//! so it must stay dependency-light: serde only, no syntect/tree-sitter.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Priority tier for a hunk. Lower number = read first.
///
/// The P-scale mirrors the user's brief: P0 is the gist — the few hunks
/// that ARE the change. P1 is supporting machinery. P2 is error handling.
/// Below P2 we're in housekeeping territory (tests, docs, chore).
///
/// 8 tiers leaves headroom without inviting inflation; empirically
/// most diffs use P0–P3 and the rest stays empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Priority {
    /// The single hunk (or small cluster) without which the change does not
    /// exist. If you only read one thing, read this.
    P0,
    /// Supporting logic the reviewer must understand to trust P0 —
    /// interface changes, new types, altered call-sites.
    P1,
    /// Error handling, rollback paths, boundary conditions that are easy
    /// to skip but load-bearing.
    P2,
    /// Tests that document intent, or assertion changes that explain
    /// expected behavior.
    P3,
    /// Documentation, comments, examples — should be skimmed for
    /// accuracy against P0 claims, not read deeply.
    P4,
    /// Mechanical changes: rename sweeps, formatting, lockfile noise,
    /// generated code. Reviewer can mostly skip after a sanity check.
    P5,
    /// Structural moves — files split/merged, wholesale relocation.
    /// Usually zero behavioral content, but needs a glance to confirm
    /// no logic snuck in with the move.
    P6,
    /// Chores: CI, build files, lint configs, version bumps.
    P7,
}

impl Priority {
    pub fn as_u8(self) -> u8 {
        match self {
            Self::P0 => 0,
            Self::P1 => 1,
            Self::P2 => 2,
            Self::P3 => 3,
            Self::P4 => 4,
            Self::P5 => 5,
            Self::P6 => 6,
            Self::P7 => 7,
        }
    }

    pub fn from_u8(n: u8) -> Option<Self> {
        match n {
            0 => Some(Self::P0),
            1 => Some(Self::P1),
            2 => Some(Self::P2),
            3 => Some(Self::P3),
            4 => Some(Self::P4),
            5 => Some(Self::P5),
            6 => Some(Self::P6),
            7 => Some(Self::P7),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::P0 => "gist",
            Self::P1 => "supporting",
            Self::P2 => "error handling",
            Self::P3 => "tests of intent",
            Self::P4 => "docs",
            Self::P5 => "mechanical",
            Self::P6 => "structural move",
            Self::P7 => "chore",
        }
    }

    pub fn all() -> [Self; 8] {
        [
            Self::P0,
            Self::P1,
            Self::P2,
            Self::P3,
            Self::P4,
            Self::P5,
            Self::P6,
            Self::P7,
        ]
    }

    /// Priority derived from a raw hunk's segments: the strongest tier
    /// among them (`as_u8` minimum — P0 wins). `None` on empty input.
    pub fn derived(hunks: &[&LabeledHunk]) -> Option<Priority> {
        hunks.iter().map(|h| h.priority).min()
    }
}

/// One labeled hunk — the atomic unit of an adhdiff.
///
/// Positions are diff rows: the 1-based line numbers of the raw diff text,
/// exactly what an editor or `sed -n` shows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabeledHunk {
    /// File path (repo-relative, forward slashes). Readability only —
    /// the entry's identity is its `row_start`.
    pub file: PathBuf,
    /// First diff row of the entry (1-indexed, inclusive). Unique across
    /// the map.
    pub row_start: u32,
    /// Last diff row of the entry (inclusive). The entry stays inside one
    /// raw hunk.
    pub row_end: u32,
    /// Agent-assigned priority.
    pub priority: Priority,
    /// One-line label — appears as the section heading on the map.
    /// E.g. "token refresh loop", "rate-limit plumbing".
    pub label: String,
    /// 1–2 sentence rationale. Required on P0, recommended on P1;
    /// lower tiers default to empty.
    #[serde(default)]
    pub rationale: String,
    /// True if this hunk is entirely mechanical (rename sweep, format).
    /// Reviewer can skim P5+ mechanical hunks in seconds.
    #[serde(default)]
    pub mechanical: bool,
    /// Agent self-reported confidence in this labeling. Renderers dim
    /// low-confidence labels; `adhdiff check --strict` promotes them.
    /// Mitigates the label-based positional bias risk: a mislabeled P5
    /// hiding a bug is worse than alphabetical order.
    #[serde(default)]
    pub confidence: Confidence,
    /// Tree edge: the `row_start` of the entry this one supports,
    /// explains, tests, or derives from. Absent = root. Roots are not
    /// tier-gated: a standalone drive-by fix at P2/P3 is its own root.
    #[serde(default)]
    pub supports: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    #[default]
    High,
    Medium,
    Low,
}

/// The full annotated map for one diff. The "artifact" the agent emits
/// and the human's reading order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdhDiffMap {
    /// Title of the change, one line. E.g. "Add retry-with-backoff to S3 upload".
    pub title: String,
    /// 2–3 sentence description of the whole change. Distinct from hunk
    /// rationales — this is the abstract, hunk rationales are per-chapter.
    pub summary: String,
    /// Content address of the raw unified diff (SHA-256 hex of its
    /// bytes). `check`, `render`, `tui` and `gui` refuse a map whose
    /// digest does not match the diff they are given.
    pub diff_sha256: String,
    /// Every hunk. Order still matters: it is the agent's recommended
    /// reading order — roots first by importance, and under each root
    /// the supporting hunks in the order that best explains it. The
    /// support relation itself is explicit, carried by each hunk's
    /// `supports` edge, not inferred from this ordering.
    pub hunks: Vec<LabeledHunk>,
}

impl AdhDiffMap {
    /// Resolve every hunk's `supports` edge to a map index in one linear
    /// pass: the target is the entry whose `row_start` equals the ref.
    /// `None` for roots (no ref) and for dangling refs; renderers degrade
    /// the latter to roots, `check` reports them as errors. First
    /// duplicate `row_start` wins.
    pub fn parent_refs(&self) -> Vec<Option<usize>> {
        let mut by_row: std::collections::HashMap<u32, usize> =
            std::collections::HashMap::with_capacity(self.hunks.len());
        for (i, h) in self.hunks.iter().enumerate() {
            by_row.entry(h.row_start).or_insert(i);
        }
        self.hunks
            .iter()
            .map(|h| by_row.get(&h.supports?).copied())
            .collect()
    }

    /// Direct children of a hunk: every hunk whose `supports` edge
    /// resolves to `idx`, ordered by (priority, map index) — the order
    /// [`reading_order`](Self::reading_order) visits them in.
    pub fn children(&self, idx: usize) -> Vec<usize> {
        let parents = self.parent_refs();
        let mut out: Vec<usize> = (0..self.hunks.len())
            .filter(|&i| parents[i] == Some(idx))
            .collect();
        out.sort_by_key(|&i| (self.hunks[i].priority, i));
        out
    }

    /// Root-first traversal of the supports forest: the map's natural
    /// reading order. Roots (no `supports` edge, or a dangling one)
    /// come in (priority, map index) order; each root is followed
    /// depth-first by its subtree, children again in (priority, map
    /// index) order. Every hunk appears exactly once — nodes trapped
    /// in a `supports` cycle (a hard `check` error) degrade to roots
    /// appended at the end.
    pub fn reading_order(&self) -> Vec<usize> {
        let n = self.hunks.len();
        let parents = self.parent_refs();
        let sorted_kids = |idx: usize| {
            let mut out: Vec<usize> = (0..n).filter(|&i| parents[i] == Some(idx)).collect();
            out.sort_by_key(|&i| (self.hunks[i].priority, i));
            out
        };
        let mut visited = vec![false; n];
        let mut out = Vec::with_capacity(n);
        let visit = |start: usize, visited: &mut [bool], out: &mut Vec<usize>| {
            let mut stack = vec![start];
            while let Some(i) = stack.pop() {
                if visited[i] {
                    continue;
                }
                visited[i] = true;
                out.push(i);
                let mut kids = sorted_kids(i);
                kids.reverse(); // LIFO pops them in forward order
                stack.extend(kids);
            }
        };

        let mut roots: Vec<usize> = (0..n).filter(|&i| parents[i].is_none()).collect();
        roots.sort_by_key(|&i| (self.hunks[i].priority, i));
        for r in roots {
            visit(r, &mut visited, &mut out);
        }
        // Anything unreachable from a root is stuck in a cycle; degrade
        // it to a root so the traversal still covers the whole map.
        let mut stuck: Vec<usize> = (0..n).filter(|&i| !visited[i]).collect();
        stuck.sort_by_key(|&i| (self.hunks[i].priority, i));
        for r in stuck {
            visit(r, &mut visited, &mut out);
        }
        out
    }

    /// How many hunks at each tier — for the map header.
    pub fn tier_counts(&self) -> Vec<(Priority, usize)> {
        Priority::all()
            .into_iter()
            .map(|p| (p, self.hunks.iter().filter(|h| h.priority == p).count()))
            .filter(|(_, n)| *n > 0)
            .collect()
    }

    /// Files touched, ordered by their highest-priority hunk.
    pub fn files_by_importance(&self) -> Vec<(&PathBuf, Priority)> {
        let mut seen = std::collections::HashMap::new();
        for h in &self.hunks {
            seen.entry(&h.file)
                .and_modify(|p: &mut Priority| {
                    if h.priority < *p {
                        *p = h.priority
                    }
                })
                .or_insert(h.priority);
        }
        let mut v: Vec<_> = seen.into_iter().collect();
        v.sort_by_key(|(_, p)| *p);
        v
    }

    /// Hunks the agent flagged as low-confidence — renderers should
    /// surface these prominently so a human double-checks the labeling.
    /// This is the main mitigation against label-based positional bias.
    pub fn low_confidence_hunks(&self) -> Vec<&LabeledHunk> {
        self.hunks
            .iter()
            .filter(|h| matches!(h.confidence, Confidence::Low))
            .collect()
    }

    /// Segments covering one raw hunk's rows: the entries fully contained
    /// in `[row_start, row_end]`, sorted by `row_start`. In a valid map
    /// they TILE the range — no gaps, no overlaps, union == range — which
    /// `check` enforces.
    pub fn segments_in(&self, row_start: u32, row_end: u32) -> Vec<&LabeledHunk> {
        let mut out: Vec<&LabeledHunk> = self
            .hunks
            .iter()
            .filter(|h| {
                h.row_start <= h.row_end && h.row_start >= row_start && h.row_end <= row_end
            })
            .collect();
        out.sort_by_key(|h| h.row_start);
        out
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedComment {
    pub id: String,
    pub file: String,
    /// Diff row the comment is anchored at.
    pub row: u32,
    /// Inclusive end row of the anchored range; absent for single-row
    /// comments. `row` stays the pin/sort anchor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_row: Option<u32>,
    pub label_at_time: String,
    pub author: String, // "human", "agent", or agent tool name
    pub body: String,
    pub ts_unix: u64,
}

// ---------- IPC wire types (`adhdiff gui` command layer) ----------

/// UI bootstrap payload returned by `get_map`: metadata only — hunk
/// headers, outline, comments, viewed flags. Heavy bodies (highlighted
/// markup) are fetched lazily per hunk via `get_hunk_body` /
/// `get_file_bodies`.
/// Single window, self-writes only: mutating commands return fresh
/// payload slices instead of events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MapPayload {
    pub title: String,
    pub summary: String,
    /// First 12 hex chars of the diff's SHA-256.
    pub diff_sha12: String,
    /// Absolute worktree path the review is bound to; absent outside a
    /// worktree (open-in-editor is disabled then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// Program of the usable `$VISUAL`/`$EDITOR`; absent when none is
    /// usable or no worktree is bound (the open-in-editor button is
    /// hidden then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,
    /// Hunks in outline (reading) order.
    pub hunks: Vec<HunkPayload>,
    pub outline: Vec<OutlineItem>,
}

/// One sub-hunk segment of a raw hunk, as it crosses IPC inside a
/// [`HunkPayload`]. Pure denormalization of the tiling map entries —
/// the frontend uses it for heatmap bands and segment affordances;
/// `tier` = `priority.as_u8()`. All fields default so older payloads
/// without segments still parse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentPayload {
    /// First diff row of the segment.
    #[serde(default)]
    pub row_start: u32,
    /// Last diff row of the segment (inclusive).
    #[serde(default)]
    pub row_end: u32,
    /// Priority tier as `as_u8` (0 = P0 … 7 = P7).
    #[serde(default)]
    pub tier: u8,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub rationale: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HunkPayload {
    /// Position in outline (reading) order.
    pub idx: usize,
    /// Index into the AdhDiffMap's hunks vec (the group's anchor entry).
    pub map_idx: usize,
    pub file: String,
    /// First body row of the raw hunk; with `file` it is the hunk's
    /// identity (viewed key, supports target).
    pub row_start: u32,
    /// Last body row of the raw hunk (inclusive).
    pub row_end: u32,
    pub priority: Priority,
    pub label: String,
    pub rationale: String,
    pub mechanical: bool,
    pub confidence: Confidence,
    /// `row_start` of the outline parent's raw hunk; `None` for roots.
    pub supports: Option<u32>,
    /// Unhighlighted body row count — the frontend reserves exactly this
    /// much vertical space (rows × line-height) until the body arrives.
    pub body_rows: u32,
    /// Segments tiling this raw hunk's rows, sorted by `row_start`;
    /// empty when the map predates sub-hunk segments.
    #[serde(default)]
    pub segments: Vec<SegmentPayload>,
    /// Leading ` `-prefixed context rows the raw diff carries; these
    /// never fold under the tier filter.
    #[serde(default)]
    pub ctx_head: u32,
    /// Trailing context rows of the raw hunk (see `ctx_head`).
    #[serde(default)]
    pub ctx_tail: u32,
    pub comments: Vec<SavedComment>,
    pub viewed: bool,
    /// The hunk's file is a regular file under the bound worktree.
    #[serde(default)]
    pub openable: bool,
}

/// One hunk's fully-rendered body: highlighted markup with comment pins
/// baked in, plus the comments covering its range. Reply of
/// `get_hunk_body`; members of the `get_file_bodies` reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HunkBody {
    /// Position in outline (reading) order.
    pub idx: usize,
    pub map_idx: usize,
    pub body_html: String,
    pub comments: Vec<SavedComment>,
}

/// Reply of `get_file_bodies`: every hunk of one file, bodies re-rendered
/// against the current comments — the comment-merge refresh slice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileBodies {
    pub file: String,
    pub bodies: Vec<HunkBody>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutlineItem {
    /// Position in outline (reading) order.
    pub idx: usize,
    pub depth: usize,
    pub label: String,
    pub commented: bool,
    pub viewed: bool,
}

/// Argument of the `add_comment` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewComment {
    pub file: String,
    pub row: u32,
    #[serde(default)]
    pub end_row: Option<u32>,
    pub body: String,
}

/// Reply of the `add_comment`/`delete_comment` commands: the file's full
/// comment list after the mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileComments {
    pub file: String,
    pub comments: Vec<SavedComment>,
}

/// Reply of the `set_viewed` command: the full viewed key set
/// (`file:row_start` identity strings, sorted).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewedState {
    pub viewed: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(p: Priority, label: &str) -> LabeledHunk {
        LabeledHunk {
            file: PathBuf::from("src/foo.rs"),
            row_start: 1,
            row_end: 10,
            priority: p,
            label: label.into(),
            rationale: String::new(),
            mechanical: false,
            confidence: Default::default(),
            supports: None,
        }
    }

    /// Hunk with an explicit row and an optional supports ref.
    fn node(p: Priority, file: &str, row: u32, supports: Option<u32>) -> LabeledHunk {
        let mut h = hunk(p, "h");
        h.file = PathBuf::from(file);
        h.row_start = row;
        h.row_end = row + 3;
        h.supports = supports;
        h
    }

    fn base_map() -> AdhDiffMap {
        AdhDiffMap {
            title: "t".into(),
            summary: "s".into(),
            diff_sha256: String::new(),
            hunks: vec![],
        }
    }

    #[test]
    fn reading_order_roots_before_children_by_tier() {
        let mut m = base_map();
        // Deliberately scrambled map order: child first, then a P2 root,
        // then the P0 root the child hangs off.
        m.hunks = vec![
            node(Priority::P1, "src/a.rs", 40, Some(10)),
            node(Priority::P2, "src/b.rs", 5, None),
            node(Priority::P0, "src/a.rs", 10, None),
        ];
        // Roots sorted by tier (P0[2] before P2[1]); the child follows
        // its parent before the next root.
        assert_eq!(m.reading_order(), vec![2, 0, 1]);
        assert_eq!(m.children(2), vec![0]);
        assert!(m.children(0).is_empty());
    }

    #[test]
    fn reading_order_children_sorted_by_tier_then_index() {
        let mut m = base_map();
        m.hunks = vec![
            node(Priority::P0, "src/a.rs", 10, None),
            node(Priority::P3, "src/a.rs", 90, Some(10)),
            node(Priority::P1, "src/a.rs", 40, Some(10)),
        ];
        assert_eq!(m.children(0), vec![2, 1]);
        assert_eq!(m.reading_order(), vec![0, 2, 1]);
    }

    #[test]
    fn reading_order_grandchild_nests_deeper() {
        let mut m = base_map();
        m.hunks = vec![
            node(Priority::P0, "src/a.rs", 10, None),
            node(Priority::P1, "src/a.rs", 40, Some(10)),
            node(Priority::P3, "src/b.rs", 5, Some(40)),
            node(Priority::P2, "src/c.rs", 1, None),
        ];
        // P3 test supports the P1 which supports the P0; the P2 root
        // comes after the whole first subtree.
        assert_eq!(m.reading_order(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn reading_order_dangling_ref_degrades_to_root() {
        let mut m = base_map();
        m.hunks = vec![
            node(Priority::P1, "src/a.rs", 40, Some(999)),
            node(Priority::P0, "src/a.rs", 10, None),
        ];
        // The P1's ref resolves nowhere: it reads as a root, sorted by tier.
        assert_eq!(m.reading_order(), vec![1, 0]);
    }

    #[test]
    fn reading_order_cycle_still_covers_every_hunk_once() {
        let mut m = base_map();
        m.hunks = vec![
            node(Priority::P0, "src/r.rs", 1, None),
            node(Priority::P1, "src/a.rs", 10, Some(20)),
            node(Priority::P2, "src/b.rs", 20, Some(10)),
        ];
        let order = m.reading_order();
        // Root first; the cycle follows (P1[1] acts as entry root),
        // each hunk exactly once.
        assert_eq!(order, vec![0, 1, 2]);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1, 2]);
    }

    #[test]
    fn tier_counts_skip_empty() {
        let mut m = base_map();
        m.hunks = vec![hunk(Priority::P0, "a"), hunk(Priority::P3, "b")];
        let cs = m.tier_counts();
        assert_eq!(cs, vec![(Priority::P0, 1), (Priority::P3, 1)]);
    }

    #[test]
    fn files_sorted_by_highest_tier() {
        let mut m = base_map();
        let mut h1 = hunk(Priority::P2, "helper");
        h1.file = PathBuf::from("src/util.rs");
        let mut h2 = hunk(Priority::P0, "core");
        h2.file = PathBuf::from("src/lib.rs");
        m.hunks = vec![h1, h2];
        let files = m.files_by_importance();
        assert_eq!(files[0].0, &PathBuf::from("src/lib.rs"));
        assert_eq!(files[1].0, &PathBuf::from("src/util.rs"));
    }

    #[test]
    fn low_confidence_hunks_surface() {
        let mut m = base_map();
        let mut h1 = hunk(Priority::P5, "rename sweep");
        h1.confidence = Confidence::Low;
        m.hunks = vec![h1];
        let risky = m.low_confidence_hunks();
        assert_eq!(risky.len(), 1);
        assert_eq!(risky[0].label, "rename sweep");
    }
}
