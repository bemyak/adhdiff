//! Review-session state: one struct of signals, each visual property with
//! exactly one reactive owner. `focused` is written by exactly three
//! handlers — keydown, click, wheel — nothing else (the constructors seed
//! it once from the loaded payload; scroll effects only ever read it).

use std::collections::{HashMap, HashSet};

use adhdiff_shared::{MapPayload, SavedComment, SegmentPayload};
use leptos::prelude::*;

#[derive(Clone, Copy)]
pub struct AppState {
    pub map: RwSignal<MapPayload>,
    /// Rendered hunk-body markup, keyed by map index. Filled lazily
    /// (visible window around `focused`) and by comment merges; entries
    /// are never evicted — the markup is what the reviewer is reading.
    pub bodies: RwSignal<HashMap<usize, String>>,
    /// Focused hunk as outline (reading-order) index.
    pub focused: RwSignal<usize>,
    /// Tier filter: max priority tier shown.
    pub tier: RwSignal<u8>,
    /// Comments-only filter: `visible` lists just hunks whose outline
    /// item carries a comment.
    pub comments_only: RwSignal<bool>,
    /// Collapsed hunk bodies, keyed by map index.
    pub collapsed: RwSignal<HashSet<usize>>,
    /// Viewed hunk keys (`file:row_start`).
    pub viewed: RwSignal<HashSet<String>>,
    pub outline_open: RwSignal<bool>,
    pub help_open: RwSignal<bool>,
    /// The open comment composer; `None` when closed. Opening a new target
    /// replaces the state outright (the popover is a singleton).
    pub composer: RwSignal<Option<ComposerState>>,
    /// Armed gutter drag (dline window being painted); `None` when idle.
    /// Never writes `focused` — drags are not focus gestures.
    pub drag: RwSignal<Option<DragState>>,
    /// One line of status text for the aria-live=polite region in the
    /// shell: written by actions with no AT-visible outcome of their own
    /// (viewed toggles, tier filter, zoom, comment add/delete). Never
    /// read by other state; the shell renders it into the live region.
    pub announce: RwSignal<String>,
    /// Hunk bodies (by map index) whose last fetch failed. Prefetch skips
    /// them until the user focuses or expands the hunk.
    pub failed: RwSignal<HashSet<usize>>,
    /// A composer add/delete call is in flight.
    pub composer_busy: RwSignal<bool>,
    /// Last composer add/delete error, shown under the textarea.
    pub composer_error: RwSignal<Option<String>>,
    /// Entry whose first row still has to be scrolled to once its body
    /// lands, with the pane `scrollTop` the navigation left behind;
    /// one-shot, dropped by the effect in lib.rs that performs it or by a
    /// user scroll / focus move.
    pub pending_scroll: RwSignal<Option<(usize, i32)>>,
    /// Native webview zoom factor. Written only by the keydown handler's
    /// zoom chords (constructor seeds it from localStorage); the apply
    /// effect in lib.rs reads it. Never reads or writes `focused`.
    pub zoom: RwSignal<f64>,
    /// Outline indices passing the tier and comments-only filters, in
    /// reading order.
    pub visible: Memo<Vec<usize>>,
    /// Tiers present in the payload (hunk priorities and segment tiers),
    /// ascending.
    pub tiers: Memo<Vec<u8>>,
}

/// The open composer's target: which hunk, which diff-row span the
/// comment covers, the row the popover anchors under, the localStorage
/// draft key for exactly this span, and the body to seed (existing
/// comment body, saved draft, or empty).
#[derive(Clone, Debug, PartialEq)]
pub struct ComposerState {
    /// Outline (reading-order) index of the anchor hunk.
    pub oi: usize,
    pub file: String,
    pub row: u32,
    /// Inclusive end; `== row` for single-row comments.
    pub end_row: u32,
    /// dline of the body row the popover anchors under; `None` anchors
    /// at the hunk head.
    pub anchor_dline: Option<u32>,
    /// `adhdiff-draft:{file}:{row}[-{end}]` — where in-progress text is
    /// persisted so Esc/blur keeps it.
    pub draft_key: String,
    pub initial_body: String,
}

/// One in-flight gutter drag, tracked in dline (hunk-local row ordinal)
/// space for painting, with the matching diff rows (`data-row`) alongside.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DragState {
    pub oi: usize,
    /// Row the press started on; the window is `min/max(anchor, hover)`.
    pub anchor_dline: u32,
    pub lo: u32,
    pub hi: u32,
    pub anchor_row: u32,
    pub lo_row: u32,
    pub hi_row: u32,
    /// Diff-row window of the composer this press just dismissed (same
    /// hunk+file): the drop closes it without reopening.
    pub close_only: Option<(u32, u32)>,
}

impl AppState {
    pub fn new(map: MapPayload) -> Self {
        let viewed: HashSet<String> = map
            .hunks
            .iter()
            .filter(|h| h.viewed)
            .map(|h| hunk_identity(&h.file, h.row_start))
            .collect();
        let collapsed: HashSet<usize> = map
            .hunks
            .iter()
            .filter(|h| h.viewed)
            .map(|h| h.map_idx)
            .collect();
        let initial_focus = map
            .hunks
            .iter()
            .find(|h| !h.viewed)
            .map(|h| h.idx)
            .unwrap_or(0);
        let n_hunks = map.hunks.len();
        let map = RwSignal::new(map);
        let bodies = RwSignal::new(HashMap::new());
        let tiers = Memo::new(move |_| {
            map.with(|m| {
                tier_set(m.hunks.iter().flat_map(|h| {
                    std::iter::once(h.priority.as_u8()).chain(h.segments.iter().map(|s| s.tier))
                }))
            })
        });
        let tier = RwSignal::new(map.with_untracked(|m| {
            m.hunks
                .iter()
                .map(|h| h.priority.as_u8())
                .max()
                .unwrap_or(0)
        }));
        let comments_only = RwSignal::new(false);
        let visible = Memo::new(move |_| {
            let max = tier.get();
            let only = comments_only.get();
            map.with(|m| {
                m.hunks
                    .iter()
                    .filter(|h| h.priority.as_u8() <= max)
                    .filter(|h| !only || m.outline[h.idx].commented)
                    .map(|h| h.idx)
                    .collect::<Vec<_>>()
            })
        });
        Self {
            map,
            bodies,
            focused: RwSignal::new(initial_focus.min(n_hunks.saturating_sub(1))),
            tier,
            comments_only,
            collapsed: RwSignal::new(collapsed),
            viewed: RwSignal::new(viewed),
            outline_open: RwSignal::new(true),
            help_open: RwSignal::new(false),
            composer: RwSignal::new(None),
            drag: RwSignal::new(None),
            announce: RwSignal::new(String::new()),
            failed: RwSignal::new(HashSet::new()),
            composer_busy: RwSignal::new(false),
            composer_error: RwSignal::new(None),
            pending_scroll: RwSignal::new(None),
            zoom: RwSignal::new(zoom_seed()),
            visible,
            tiers,
        }
    }

    /// `focused` writer 1 of 3 — the keydown handler (j/k/J/K). Scrolls
    /// the hunk head + outline row into view. The view-and-advance (`v`)
    /// sets `focused` itself and defers the scroll past the fold flush:
    /// the mark collapses the hunk, dragging every hunk below up, so a
    /// synchronous scroll would target pre-fold geometry.
    pub fn key_focus(&self, oi: usize) {
        self.clear_failed(oi);
        self.focused.set(oi);
        self.navigate(oi);
    }

    /// Scroll to entry `oi`'s first row; when its body has not landed yet
    /// the head is shown meanwhile and the row scroll is left pending.
    pub fn navigate(&self, oi: usize) {
        if !crate::scroll_focus_into_view(oi) {
            let top = crate::detail_pane().map_or(0, |p| p.scroll_top());
            self.pending_scroll.set(Some((oi, top)));
        }
    }

    /// `focused` writer 2 of 3 — the click handler (hunk cards, outline
    /// items, and the eye button's mark-viewed advance). Never scrolls:
    /// the pointer is already at the looked-at position, so scrolling the
    /// hunk head to the pane top on a click deep inside a long article
    /// yanked the viewport back to the head. Callers that navigate TO a
    /// hunk (outline rows) scroll explicitly at the callsite.
    pub fn click_focus(&self, oi: usize) {
        self.clear_failed(oi);
        self.focused.set(oi);
    }

    /// `focused` writer 3 of 3 — the wheel handler's rAF tick. Never
    /// scrolls: the pane is already where the user's wheel put it.
    pub fn wheel_focus(&self, oi: usize) {
        self.focused.set(oi);
    }

    /// An explicit focus or expand gesture on hunk `oi` re-arms its body
    /// fetch once after a failure.
    pub fn clear_failed(&self, oi: usize) {
        let Some(mi) = self
            .map
            .with_untracked(|m| m.hunks.get(oi).map(|h| h.map_idx))
        else {
            return;
        };
        if self.failed.with_untracked(|f| f.contains(&mi)) {
            self.failed.update(|f| _ = f.remove(&mi));
        }
    }

    /// Write the live region. The same text twice would not be spoken
    /// again, so a repeat gets a trailing zero-width space.
    pub fn say(&self, msg: impl Into<String>) {
        let mut msg = msg.into();
        if self.announce.with_untracked(|cur| *cur == msg) {
            msg.push('\u{200b}');
        }
        self.announce.set(msg);
    }
}

/// Row an outline entry navigates to: the first row of its strongest
/// segment (earliest on ties), the raw hunk's first row without segments.
pub fn entry_target_row(row_start: u32, segments: &[SegmentPayload]) -> u32 {
    segments
        .iter()
        .min_by_key(|s| (s.tier, s.row_start))
        .map_or(row_start, |s| s.row_start)
}

/// `scrollTop` that puts a target `target_top` px down the pane's scroll
/// space one third of the viewport from the top, clamped to the scrollable
/// range.
pub fn scroll_top_for(target_top: f64, client_height: f64, scroll_height: f64) -> f64 {
    (target_top - client_height / 3.0).clamp(0.0, (scroll_height - client_height).max(0.0))
}

pub fn hunk_identity(file: &str, row_start: u32) -> String {
    format!("{file}:{row_start}")
}

pub const DEFAULT_ZOOM: f64 = 1.0;
/// localStorage key for the persisted zoom factor (plain float string).
pub const ZOOM_STORAGE_KEY: &str = "adhdiff-zoom";

/// Chromium-style zoom ladder: zoom in/out snaps to the neighboring rung
/// instead of accumulating float multipliers; reset is the middle rung.
/// Bounds match wry's usable range on WebKitGTK.
const ZOOM_LEVELS: &[f64] = &[
    0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0, 5.0,
];

/// First rung above `cur` (epsilon-tolerant: stored floats survive a
/// string roundtrip).
pub fn zoom_in(cur: f64) -> f64 {
    ZOOM_LEVELS
        .iter()
        .copied()
        .find(|&l| l > cur + 1e-9)
        .unwrap_or(*ZOOM_LEVELS.last().unwrap())
}

/// Last rung below `cur`; clamps at the bottom rung.
pub fn zoom_out(cur: f64) -> f64 {
    ZOOM_LEVELS
        .iter()
        .rev()
        .copied()
        .find(|&l| l < cur - 1e-9)
        .unwrap_or(ZOOM_LEVELS[0])
}

/// Persisted zoom seed; anything unreadable/unparseable falls back to
/// 100%. localStorage only exists in the webview — host tests never
/// construct AppState.
fn zoom_seed() -> f64 {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(ZOOM_STORAGE_KEY).ok().flatten())
        .and_then(|v| parse_zoom(&v))
        .unwrap_or(DEFAULT_ZOOM)
}

/// A stored zoom factor clamped to the ladder's range, like `zoom_in` /
/// `zoom_out` keep it.
fn parse_zoom(raw: &str) -> Option<f64> {
    raw.parse::<f64>()
        .ok()
        .filter(|z| z.is_finite() && *z > 0.0)
        .map(|z| z.clamp(ZOOM_LEVELS[0], ZOOM_LEVELS[ZOOM_LEVELS.len() - 1]))
}

/// Wheel-derived focus: index in `heads` of the last hunk whose head is at
/// or above the viewport anchor line, defaulting to the first. A long hunk
/// spanning the anchor keeps focus because the next hunk's head sits below
/// it. Pure geometry — no DOM — so host tests can pin the behavior.
pub fn derive_focus(heads: &[f64], anchor: f64) -> Option<usize> {
    if heads.is_empty() {
        return None;
    }
    let mut best = 0;
    for (i, &head) in heads.iter().enumerate() {
        if head <= anchor {
            best = i;
        }
    }
    Some(best)
}

/// Step `d` (±1) within the visible outline indices from `cur`. `cur` may
/// be filtered out; stepping then lands on the first visible index past it.
pub fn step_visible(visible: &[usize], cur: usize, d: i64) -> Option<usize> {
    if visible.is_empty() {
        return None;
    }
    let pos = visible.partition_point(|&v| v < cur);
    let at = if visible.get(pos) == Some(&cur) {
        pos as i64 + d
    } else if d > 0 {
        pos as i64
    } else {
        pos as i64 - 1
    };
    if at < 0 || at >= visible.len() as i64 {
        return None;
    }
    Some(visible[at as usize])
}

/// Inputs of one wheel tick for [`decide_wheel_focus`].
#[derive(Clone, Copy, Debug)]
pub struct WheelTick {
    pub geometry_moved: bool,
    pub derived: usize,
    pub cur: usize,
    pub dy: f64,
    pub at_top: bool,
    pub at_bottom: bool,
    pub scrollable: bool,
    pub first: usize,
    pub last: usize,
}

/// One wheel tick's focus decision. Derivation must win only when the wheel
/// actually moved the pane (`geometry_moved`); with a static pane — clamped
/// at an edge, or right after a programmatic scroll re-stored the offset —
/// stale head geometry would otherwise snap focus back on every tick. Even
/// on a moving tick, a push against the resting edge (down at the bottom,
/// up at the top) settles focus on `last`/`first`: the midline rule parks
/// one hunk short of the extreme when several small hunks fit past it, and
/// the clamped pane yields no further moving ticks that would re-derive.
/// The settle is gated on `scrollable`: when the whole document fits the
/// pane both edges read true, and a spurious `geometry_moved` (e.g. the
/// first tick, whose pane offset baseline is unseeded) must not leap focus
/// to an extreme with zero pane motion.
/// Failing that, a wheel push past the clamped edge advances one visible
/// hunk (`step` maps ±1 to the next visible index) and asks the caller to
/// scroll it into view. Returns `(index, scroll)`; `None` keeps `focused`.
pub fn decide_wheel_focus(
    WheelTick {
        geometry_moved,
        derived,
        cur,
        dy,
        at_top,
        at_bottom,
        scrollable,
        first,
        last,
    }: WheelTick,
    step: impl Fn(i64) -> Option<usize>,
) -> Option<(usize, bool)> {
    if geometry_moved && derived != cur {
        if scrollable && dy > 0.0 && at_bottom {
            return Some((last, false));
        }
        if scrollable && dy < 0.0 && at_top {
            return Some((first, false));
        }
        return Some((derived, false));
    }
    if (dy > 0.0 && at_bottom) || (dy < 0.0 && at_top) {
        let d = if dy > 0.0 { 1 } else { -1 };
        if let Some(a) = step(d)
            && a != cur
        {
            return Some((a, true));
        }
    }
    None
}

/// Step ±1 among the visible, commented outline indices, wrapping around.
/// `cur` need not be in the list; wrapping matches the old JS ring walk.
pub fn step_commented(commented: &[usize], cur: usize, d: i64) -> Option<usize> {
    if commented.is_empty() {
        return None;
    }
    if d > 0 {
        Some(
            commented
                .iter()
                .find(|&&v| v > cur)
                .copied()
                .unwrap_or(commented[0]),
        )
    } else {
        Some(
            commented
                .iter()
                .rev()
                .find(|&&v| v < cur)
                .copied()
                .unwrap_or(commented[commented.len() - 1]),
        )
    }
}

/// Where focus lands after marking hunk `from` viewed: the next visible
/// hunk, wrapping to the first when none follow.
pub fn advance_after_viewed(visible: &[usize], from: usize) -> Option<usize> {
    if visible.is_empty() {
        return None;
    }
    let pos = visible.partition_point(|&v| v <= from);
    Some(visible.get(pos).copied().unwrap_or(visible[0]))
}

/// Ascending, deduplicated tiers from hunk priorities and segment tiers.
pub fn tier_set(tiers: impl IntoIterator<Item = u8>) -> Vec<u8> {
    let mut t: Vec<u8> = tiers.into_iter().collect();
    t.sort_unstable();
    t.dedup();
    t
}

/// Context kept visible around each surviving row when the tier filter
/// folds stage-2 segments out of a loaded body (± rows, clamped).
pub const FOLD_CONTEXT_ROWS: usize = 3;

/// Inclusive row-index runs to hide under a stage-2 tier fold. A row
/// survives when its tier is `None` (outside every segment — always
/// visible) or `<= max_tier`; each survivor keeps `ctx` context rows on
/// both sides, clamped to the body. The complement of the kept set,
/// merged into maximal runs, is what folds. No survivors (unreachable
/// for a hunk that passed stage 1, whose derived tier IS one of its
/// segments' tiers) folds the whole body: one run over every row.
pub fn fold_runs(row_tiers: &[Option<u8>], max_tier: u8, ctx: usize) -> Vec<(usize, usize)> {
    let n = row_tiers.len();
    if n == 0 {
        return Vec::new();
    }
    let mut keep = vec![false; n];
    for (i, tier) in row_tiers.iter().enumerate() {
        if tier.is_none_or(|t| t <= max_tier) {
            let lo = i.saturating_sub(ctx);
            let hi = (i + ctx).min(n - 1);
            keep[lo..=hi].fill(true);
        }
    }
    let mut runs = Vec::new();
    let mut start = None;
    for (i, &kept) in keep.iter().enumerate() {
        match (kept, start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                runs.push((s, i - 1));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((s, n - 1));
    }
    runs
}

/// One fold seam inside a body: rows `start..=end` (inclusive dline
/// ordinals) hide. Commented rows never appear in a plan: [`seam_plan`]
/// splits every run around them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeamPlan {
    pub start: usize,
    pub end: usize,
}

/// Hide set + seam list for one body: [`fold_runs`] with every commented
/// or pinned row rescued — such a row never folds; it splits the run
/// around it and stays visible alone (no added context, edge rows
/// simply trim the run, a run of only rescued rows emits no seams).
/// `commented` is parallel to `row_tiers`; a short slice counts missing
/// rows as unrescued.
pub fn seam_plan(
    row_tiers: &[Option<u8>],
    commented: &[bool],
    max_tier: u8,
    ctx: usize,
) -> Vec<SeamPlan> {
    let mut plans = Vec::new();
    for (start, end) in fold_runs(row_tiers, max_tier, ctx) {
        let mut lo = start;
        for i in start..=end {
            if commented.get(i).copied().unwrap_or(false) {
                if i > lo {
                    plans.push(SeamPlan {
                        start: lo,
                        end: i - 1,
                    });
                }
                lo = i + 1;
            }
        }
        if end >= lo {
            plans.push(SeamPlan { start: lo, end });
        }
    }
    plans
}

/// Marks the raw hunk's own context rows (`lead` first, `trail` last) as
/// pinned: [`seam_plan`] never folds a pinned row.
pub fn pin_context(pinned: &mut [bool], lead: usize, trail: usize) {
    let n = pinned.len();
    pinned[..lead.min(n)].fill(true);
    pinned[n - trail.min(n)..].fill(true);
}

/// Per-row tiers for one raw hunk from its tiling segments: row
/// `first_row + i` takes the tier of the segment whose span contains
/// it; `None` when no segment claims it ([`fold_runs`] treats `None` as
/// always visible). DOM-free mirror of the baked `seg-p{N}` row classes.
pub fn segment_row_tiers(
    segments: &[SegmentPayload],
    first_row: u32,
    n_rows: usize,
) -> Vec<Option<u8>> {
    (0..n_rows)
        .map(|i| {
            let row = first_row.saturating_add(i as u32);
            segments
                .iter()
                .find(|s| s.row_start <= row && row <= s.row_end)
                .map(|s| s.tier)
        })
        .collect()
}

/// Rows still visible in a `body_rows`-row body under `max_tier`: fold
/// runs computed over the hunk's rows (segments are row spans, so the
/// estimate matches the DOM's per-row classes).
pub fn visible_row_estimate(
    segments: &[SegmentPayload],
    row_start: u32,
    row_end: u32,
    body_rows: u32,
    (ctx_head, ctx_tail): (u32, u32),
    max_tier: u8,
) -> u32 {
    let Some(n_rows) = row_end.checked_sub(row_start).map(|d| d as usize + 1) else {
        return body_rows;
    };
    let tiers = segment_row_tiers(segments, row_start, n_rows);
    let mut pinned = vec![false; n_rows];
    pin_context(&mut pinned, ctx_head as usize, ctx_tail as usize);
    let hidden: usize = seam_plan(&tiers, &pinned, max_tier, FOLD_CONTEXT_ROWS)
        .iter()
        .map(|p| p.end - p.start + 1)
        .sum();
    body_rows.saturating_sub(hidden as u32)
}

/// localStorage draft key for a comment span; ranges add the end row.
pub fn draft_key(file: &str, row: u32, end_row: u32) -> String {
    if end_row > row {
        format!("adhdiff-draft:{file}:{row}-{end_row}")
    } else {
        format!("adhdiff-draft:{file}:{row}")
    }
}

/// First stored comment in `file` whose span overlaps `[row, end_row]`
/// — the old "one comment per line" scan over every hunk's comment list
/// in rendering order. The composer adopts its span and prefills its body
/// so submitting overwrites it in place.
pub fn find_overlap<'a>(
    comments: impl IntoIterator<Item = &'a SavedComment>,
    file: &str,
    row: u32,
    end_row: u32,
) -> Option<&'a SavedComment> {
    comments
        .into_iter()
        .find(|c| c.file == file && c.row <= end_row && c.end_row.unwrap_or(c.row) >= row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_target_is_the_strongest_segments_first_row() {
        let seg = |row_start, tier| SegmentPayload {
            row_start,
            row_end: row_start + 5,
            tier,
            label: String::new(),
            rationale: String::new(),
        };
        let segs = [seg(10, 6), seg(20, 1), seg(30, 0), seg(40, 0), seg(50, 1)];
        assert_eq!(entry_target_row(10, &segs), 30);
        assert_eq!(entry_target_row(10, &[]), 10);
    }

    #[test]
    fn scroll_top_sits_a_third_down_and_clamps() {
        assert_eq!(scroll_top_for(1000.0, 600.0, 5000.0), 800.0);
        assert_eq!(scroll_top_for(100.0, 600.0, 5000.0), 0.0);
        assert_eq!(scroll_top_for(4900.0, 600.0, 5000.0), 4400.0);
        assert_eq!(scroll_top_for(300.0, 600.0, 400.0), 0.0);
    }

    #[test]
    fn derive_focus_empty_is_none() {
        assert_eq!(derive_focus(&[], 100.0), None);
    }

    #[test]
    fn derive_focus_picks_last_head_at_or_above_anchor() {
        // heads at 10, 300, 700; anchor midline 450 → idx 1 (head 300)
        assert_eq!(derive_focus(&[10.0, 300.0, 700.0], 450.0), Some(1));
        // anchor exactly on a head counts as "at" it
        assert_eq!(derive_focus(&[10.0, 300.0, 700.0], 300.0), Some(1));
        // past the last head → the last hunk (bottom clamp)
        assert_eq!(derive_focus(&[10.0, 300.0, 700.0], 900.0), Some(2));
    }

    #[test]
    fn derive_focus_all_heads_below_anchor_stays_at_first() {
        assert_eq!(derive_focus(&[500.0, 600.0], 100.0), Some(0));
    }

    #[test]
    fn derive_focus_long_hunk_spanning_anchor_keeps_focus() {
        // Focused hunk's head scrolled far above the viewport (negative
        // top); the next hunk's head is far below the anchor. Scrolling
        // inside the long hunk must not move focus to the next hunk.
        assert_eq!(derive_focus(&[-2000.0, 5000.0], 450.0), Some(0));
        // ...not even when only the long hunk's tail fills the viewport.
        assert_eq!(derive_focus(&[-50.0, 900.0], 100.0), Some(0));
    }

    #[test]
    fn collapsed_all_walk_up_then_down_monotonic() {
        // Every hunk collapsed: the pane can't scroll (both edges at once),
        // all heads sit above the anchor so derivation would say 2 on every
        // tick. The walk must still march monotonically and never flip
        // back to the derived index.
        let vis = [0, 1, 2];
        let mut cur = 2;
        let mut seq = vec![cur];
        for _ in 0..8 {
            if let Some((next, scroll)) = decide_wheel_focus(
                WheelTick {
                    geometry_moved: false,
                    derived: 2,
                    cur,
                    dy: -120.0,
                    at_top: true,
                    at_bottom: true,
                    scrollable: false,
                    first: 0,
                    last: 2,
                },
                |d| step_visible(&vis, cur, d),
            ) {
                assert!(scroll);
                cur = next;
            }
            seq.push(cur);
        }
        assert!(seq.windows(2).all(|w| w[1] <= w[0]), "up-walk: {seq:?}");
        assert_eq!(*seq.last().unwrap(), 0);
        let mut seq = vec![cur];
        for _ in 0..8 {
            if let Some((next, scroll)) = decide_wheel_focus(
                WheelTick {
                    geometry_moved: false,
                    derived: 2,
                    cur,
                    dy: 120.0,
                    at_top: true,
                    at_bottom: true,
                    scrollable: false,
                    first: 0,
                    last: 2,
                },
                |d| step_visible(&vis, cur, d),
            ) {
                assert!(scroll);
                cur = next;
            }
            seq.push(cur);
        }
        assert!(seq.windows(2).all(|w| w[1] >= w[0]), "down-walk: {seq:?}");
        assert_eq!(*seq.last().unwrap(), 2);
    }

    #[test]
    fn derivation_overrides_when_geometry_moved() {
        assert_eq!(
            decide_wheel_focus(
                WheelTick {
                    geometry_moved: true,
                    derived: 5,
                    cur: 2,
                    dy: -120.0,
                    at_top: false,
                    at_bottom: false,
                    scrollable: true,
                    first: 0,
                    last: 9
                },
                |_| None
            ),
            Some((5, false))
        );
        // Wheeling along an edge but AWAY from it is not an edge push:
        // derivation still wins.
        assert_eq!(
            decide_wheel_focus(
                WheelTick {
                    geometry_moved: true,
                    derived: 5,
                    cur: 2,
                    dy: -120.0,
                    at_top: false,
                    at_bottom: true,
                    scrollable: true,
                    first: 0,
                    last: 9
                },
                |_| None
            ),
            Some((5, false))
        );
        assert_eq!(
            decide_wheel_focus(
                WheelTick {
                    geometry_moved: true,
                    derived: 5,
                    cur: 2,
                    dy: 120.0,
                    at_top: true,
                    at_bottom: false,
                    scrollable: true,
                    first: 0,
                    last: 9
                },
                |_| None
            ),
            Some((5, false))
        );
    }

    #[test]
    fn edge_advance_clamps_to_none() {
        let vis = [0, 1, 2];
        // At the last visible hunk with the wheel pushing down past the
        // clamped bottom edge: no focus change at all.
        assert_eq!(
            decide_wheel_focus(
                WheelTick {
                    geometry_moved: false,
                    derived: 2,
                    cur: 2,
                    dy: 120.0,
                    at_top: false,
                    at_bottom: true,
                    scrollable: true,
                    first: 0,
                    last: 2
                },
                |d| { step_visible(&vis, 2, d) }
            ),
            None
        );
    }

    #[test]
    fn advance_does_not_retrigger_derivation() {
        let vis = [0, 1, 2, 3, 4];
        // Clamped at the bottom: the wheel advances one visible hunk and
        // the tick scrolls it into view, re-storing the pane offset.
        let first = decide_wheel_focus(
            WheelTick {
                geometry_moved: false,
                derived: 2,
                cur: 2,
                dy: 120.0,
                at_top: false,
                at_bottom: true,
                scrollable: true,
                first: 0,
                last: 4,
            },
            |d| step_visible(&vis, 2, d),
        );
        assert_eq!(first, Some((3, true)));
        // Next tick: the offset re-store means geometry_moved=false while
        // the heads haven't settled (derived 2 < cur). No snap-back; the
        // wheel keeps advancing.
        let second = decide_wheel_focus(
            WheelTick {
                geometry_moved: false,
                derived: 2,
                cur: 3,
                dy: 120.0,
                at_top: false,
                at_bottom: true,
                scrollable: true,
                first: 0,
                last: 4,
            },
            |d| step_visible(&vis, 3, d),
        );
        assert_eq!(second, Some((4, true)));
    }

    #[test]
    fn moving_tick_at_bottom_reaches_last_visible() {
        let vis = [0, 1, 2, 3];
        // Small trailing hunks with the pane clamped at the bottom: the
        // last head sits below the midline, so derivation (2) parks
        // one-before-last. The moving down-push still settles on the last
        // visible hunk, without a scroll.
        assert_eq!(
            decide_wheel_focus(
                WheelTick {
                    geometry_moved: true,
                    derived: 2,
                    cur: 1,
                    dy: 120.0,
                    at_top: false,
                    at_bottom: true,
                    scrollable: true,
                    first: 0,
                    last: 3
                },
                |d| { step_visible(&vis, 1, d) }
            ),
            Some((3, false))
        );
        // Focus already on the last hunk: no snap-back to the derive.
        assert_eq!(
            decide_wheel_focus(
                WheelTick {
                    geometry_moved: true,
                    derived: 2,
                    cur: 3,
                    dy: 120.0,
                    at_top: false,
                    at_bottom: true,
                    scrollable: true,
                    first: 0,
                    last: 3
                },
                |d| { step_visible(&vis, 3, d) }
            ),
            Some((3, false))
        );
    }

    #[test]
    fn moving_tick_at_top_reaches_first_visible() {
        let vis = [0, 1, 2, 3];
        // Mirror at the top: several small heads all sit above the
        // midline, derivation says 2; the moving up-push settles on the
        // first visible hunk.
        assert_eq!(
            decide_wheel_focus(
                WheelTick {
                    geometry_moved: true,
                    derived: 2,
                    cur: 3,
                    dy: -120.0,
                    at_top: true,
                    at_bottom: false,
                    scrollable: true,
                    first: 0,
                    last: 3
                },
                |d| { step_visible(&vis, 3, d) }
            ),
            Some((0, false))
        );
        assert_eq!(
            decide_wheel_focus(
                WheelTick {
                    geometry_moved: true,
                    derived: 1,
                    cur: 0,
                    dy: -120.0,
                    at_top: true,
                    at_bottom: false,
                    scrollable: true,
                    first: 0,
                    last: 3
                },
                |d| { step_visible(&vis, 0, d) }
            ),
            Some((0, false))
        );
    }

    #[test]
    fn unscrollable_pane_keeps_derivation() {
        let vis = [0, 1, 2, 3];
        // The document fits the pane: both edges read true and the first
        // wheel tick reports moved=true only because the pane offset
        // baseline is unseeded. An edge settle here would leap focus to
        // an extreme with zero pane motion; derivation must win instead.
        assert_eq!(
            decide_wheel_focus(
                WheelTick {
                    geometry_moved: true,
                    derived: 2,
                    cur: 1,
                    dy: 120.0,
                    at_top: true,
                    at_bottom: true,
                    scrollable: false,
                    first: 0,
                    last: 3
                },
                |d| { step_visible(&vis, 1, d) }
            ),
            Some((2, false))
        );
        assert_eq!(
            decide_wheel_focus(
                WheelTick {
                    geometry_moved: true,
                    derived: 2,
                    cur: 3,
                    dy: -120.0,
                    at_top: true,
                    at_bottom: true,
                    scrollable: false,
                    first: 0,
                    last: 3
                },
                |d| { step_visible(&vis, 3, d) }
            ),
            Some((2, false))
        );
    }

    #[test]
    fn step_visible_walks_only_visible() {
        let vis = [0, 2, 5];
        assert_eq!(step_visible(&vis, 0, 1), Some(2));
        assert_eq!(step_visible(&vis, 2, 1), Some(5));
        assert_eq!(step_visible(&vis, 5, 1), None);
        assert_eq!(step_visible(&vis, 0, -1), None);
        // Current filtered out: step lands past it.
        assert_eq!(step_visible(&vis, 1, 1), Some(2));
        assert_eq!(step_visible(&vis, 3, -1), Some(2));
    }

    #[test]
    fn step_commented_wraps() {
        let c = [1, 4, 6];
        assert_eq!(step_commented(&c, 4, 1), Some(6));
        assert_eq!(step_commented(&c, 6, 1), Some(1));
        assert_eq!(step_commented(&c, 1, -1), Some(6));
        assert_eq!(step_commented(&c, 3, 1), Some(4));
        assert_eq!(step_commented(&c, 5, -1), Some(4));
    }

    #[test]
    fn advance_after_viewed_takes_next_visible_else_first() {
        let vis = [0, 2, 5];
        assert_eq!(advance_after_viewed(&vis, 2), Some(5));
        assert_eq!(advance_after_viewed(&vis, 5), Some(0));
        assert_eq!(advance_after_viewed(&vis, 0), Some(2));
    }

    #[test]
    fn draft_keys_match_old_scheme() {
        assert_eq!(draft_key("src/a.rs", 3, 3), "adhdiff-draft:src/a.rs:3");
        assert_eq!(draft_key("src/a.rs", 3, 7), "adhdiff-draft:src/a.rs:3-7");
        // Deflated end (<= start) collapses to the single-line key.
        assert_eq!(draft_key("src/a.rs", 3, 1), "adhdiff-draft:src/a.rs:3");
    }

    fn comment(file: &str, start: u32, end: Option<u32>, body: &str) -> SavedComment {
        SavedComment {
            id: format!("{file}:{start}"),
            file: file.into(),
            row: start,
            end_row: end,
            label_at_time: "x".into(),
            author: "human".into(),
            body: body.into(),
            ts_unix: 0,
        }
    }

    #[test]
    fn overlap_finds_first_covering_comment() {
        let cs = [
            comment("a.rs", 2, Some(4), "first"),
            comment("a.rs", 9, None, "second"),
            comment("b.rs", 3, None, "other file"),
        ];
        // Touching either edge counts as overlap (inclusive spans).
        assert_eq!(
            find_overlap(&cs, "a.rs", 4, 6).map(|c| &c.body[..]),
            Some("first")
        );
        assert_eq!(
            find_overlap(&cs, "a.rs", 1, 2).map(|c| &c.body[..]),
            Some("first")
        );
        assert_eq!(
            find_overlap(&cs, "a.rs", 9, 9).map(|c| &c.body[..]),
            Some("second")
        );
        assert_eq!(find_overlap(&cs, "a.rs", 5, 8), None);
        assert_eq!(
            find_overlap(&cs, "b.rs", 3, 3).map(|c| &c.body[..]),
            Some("other file")
        );
    }

    #[test]
    fn stored_zoom_is_clamped_to_the_ladder() {
        assert_eq!(parse_zoom("1.5"), Some(1.5));
        assert_eq!(parse_zoom("100"), Some(5.0));
        assert_eq!(parse_zoom("0.01"), Some(0.25));
        assert_eq!(parse_zoom("0"), None);
        assert_eq!(parse_zoom("NaN"), None);
        assert_eq!(parse_zoom("x"), None);
    }

    #[test]
    fn zoom_snaps_to_ladder_rungs() {
        assert_eq!(zoom_in(1.0), 1.1);
        assert_eq!(zoom_in(1.1), 1.25);
        assert_eq!(zoom_out(1.0), 0.9);
        assert_eq!(zoom_out(DEFAULT_ZOOM), 0.9);
        // Mid-rung floats snap strictly by direction.
        assert_eq!(zoom_in(1.05), 1.1);
        assert_eq!(zoom_out(1.05), 1.0);
        // Ends clamp.
        assert_eq!(zoom_in(5.0), 5.0);
        assert_eq!(zoom_out(0.25), 0.25);
        // A persisted string roundtrip keeps rung identity.
        assert_eq!(zoom_in("1.1".parse::<f64>().unwrap()), 1.25);
        assert_eq!(zoom_out("0.33".parse::<f64>().unwrap()), 0.25);
    }

    fn tiers(table: &[Option<u8>]) -> Vec<Option<u8>> {
        table.to_vec()
    }

    #[test]
    fn fold_empty_body_has_no_runs() {
        assert_eq!(fold_runs(&[], 0, 3), Vec::<(usize, usize)>::new());
    }

    #[test]
    fn fold_no_survivors_hides_everything() {
        // Whole-hunk disappearances are stage 1's job, but if every row
        // sits above the filter the body folds completely — one full run,
        // no stray context.
        assert_eq!(
            fold_runs(&tiers(&[Some(4), Some(5), Some(6)]), 0, 3),
            vec![(0, 2)]
        );
    }

    #[test]
    fn fold_none_tier_rows_always_survive() {
        // Unclaimed rows (pure-deletion bodies, fallback paths) never fold.
        assert_eq!(
            fold_runs(&tiers(&[None, None]), 0, 0),
            Vec::<(usize, usize)>::new()
        );
        assert_eq!(
            fold_runs(&tiers(&[Some(5), None, Some(5)]), 0, 0),
            vec![(0, 0), (2, 2)]
        );
    }

    #[test]
    fn fold_survivors_keep_context_clamped_to_edges() {
        // Survivor at row 0: the ±3 halo clamps to the body start.
        let t: Vec<Option<u8>> = std::iter::once(Some(0))
            .chain(std::iter::repeat_n(Some(7), 9))
            .collect();
        assert_eq!(fold_runs(&t, 0, FOLD_CONTEXT_ROWS), vec![(4, 9)]);
        // Survivor at the last row: halo clamps to the body end.
        let t: Vec<Option<u8>> = std::iter::repeat_n(Some(7), 9)
            .chain(std::iter::once(Some(0)))
            .collect();
        assert_eq!(fold_runs(&t, 0, FOLD_CONTEXT_ROWS), vec![(0, 5)]);
    }

    #[test]
    fn fold_adjacent_hidden_rows_merge_into_one_run() {
        // Adjacent folded rows merge into ONE seam run.
        assert_eq!(
            fold_runs(&tiers(&[Some(5), Some(5), Some(0)]), 0, 0),
            vec![(0, 1)]
        );
        // Context halos shrink the band between survivors: ctx=0 folds it
        // whole, ctx=1 leaves only the middle row.
        let t = tiers(&[Some(0), Some(5), Some(5), Some(5), Some(0)]);
        assert_eq!(fold_runs(&t, 0, 0), vec![(1, 3)]);
        assert_eq!(fold_runs(&t, 0, 1), vec![(2, 2)]);
    }

    #[test]
    fn fold_ctx_zero_hides_every_ineligible_row() {
        assert_eq!(
            fold_runs(&tiers(&[Some(2), Some(1), Some(3)]), 1, 0),
            vec![(0, 0), (2, 2)]
        );
    }

    #[test]
    fn seam_plan_rescues_commented_rows_splitting_the_run() {
        // 10 folded rows with comments at rows 2 and 9 → the run splits
        // into three comment-free sub-runs; the commented rows stay
        // visible alone between them.
        let t = tiers(&[
            Some(0),
            Some(5),
            Some(5),
            Some(5),
            Some(5),
            Some(5),
            Some(5),
            Some(5),
            Some(5),
            Some(5),
            Some(5),
            Some(0),
        ]);
        let commented = [
            false, false, true, false, false, false, false, false, false, true, false, false,
        ];
        assert_eq!(
            seam_plan(&t, &commented, 0, 0),
            vec![
                SeamPlan { start: 1, end: 1 },
                SeamPlan { start: 3, end: 8 },
                SeamPlan { start: 10, end: 10 },
            ]
        );
    }

    #[test]
    fn seam_plan_trims_comments_at_run_edges() {
        // A comment at either folded edge trims the run — no empty
        // sub-run is emitted for the rescued end.
        let t = tiers(&[Some(5), Some(5), Some(5)]);
        assert_eq!(
            seam_plan(&t, &[true, false, false], 0, 0),
            vec![SeamPlan { start: 1, end: 2 }]
        );
        assert_eq!(
            seam_plan(&t, &[false, false, true], 0, 0),
            vec![SeamPlan { start: 0, end: 1 }]
        );
    }

    #[test]
    fn seam_plan_adjacent_comments_stay_visible_alone() {
        // Adjacent commented rows rescue independently — both visible —
        // with the folded sub-runs intact on each side.
        let t = tiers(&[Some(5), Some(5), Some(5), Some(5), Some(5)]);
        let commented = [false, true, true, false, false];
        assert_eq!(
            seam_plan(&t, &commented, 0, 0),
            vec![SeamPlan { start: 0, end: 0 }, SeamPlan { start: 3, end: 4 }]
        );
    }

    #[test]
    fn seam_plan_all_commented_run_emits_no_seams() {
        // A run of nothing but commented rows folds nothing.
        let t = tiers(&[Some(5), Some(5)]);
        assert_eq!(seam_plan(&t, &[true, true], 0, 0), Vec::<SeamPlan>::new());
    }

    #[test]
    fn seam_plan_comment_adjacent_to_survivor_trims_only() {
        // A comment right after a surviving row is rescued; only the rows
        // truly past it fold (the survivor's side emits no empty run).
        let t = tiers(&[Some(0), Some(5), Some(5)]);
        let commented = [false, true, false];
        assert_eq!(
            seam_plan(&t, &commented, 0, 0),
            vec![SeamPlan { start: 2, end: 2 }]
        );
        // Missing entries in a short `commented` slice rescue nothing.
        let t = tiers(&[Some(5), Some(0), Some(5)]);
        assert_eq!(
            seam_plan(&t, &[true], 0, 0),
            vec![SeamPlan { start: 2, end: 2 }]
        );
    }

    #[test]
    fn seam_plan_rescue_adds_no_context() {
        // Survivor context is unchanged (rows 1..=3 stay for the survivor
        // at 0) and the rescued row gets NO ±ctx halo: the comment at 6
        // splits the raw run 4..=9 without widening anything around it.
        let t: Vec<Option<u8>> = std::iter::once(Some(0))
            .chain(std::iter::repeat_n(Some(7), 9))
            .collect();
        let mut commented = [false; 10];
        commented[6] = true;
        assert_eq!(
            seam_plan(&t, &commented, 0, FOLD_CONTEXT_ROWS),
            vec![SeamPlan { start: 4, end: 5 }, SeamPlan { start: 7, end: 9 }]
        );
        // Sanity: uncommented, the same body folds as one run, matching
        // fold_survivors_keep_context_clamped_to_edges.
        assert_eq!(
            seam_plan(&t, &[false; 10], 0, FOLD_CONTEXT_ROWS),
            vec![SeamPlan { start: 4, end: 9 }]
        );
    }

    fn seg(row_start: u32, row_end: u32, tier: u8) -> SegmentPayload {
        SegmentPayload {
            row_start,
            row_end,
            tier,
            label: String::new(),
            rationale: String::new(),
        }
    }

    #[test]
    fn tier_set_sorts_and_dedups() {
        assert_eq!(tier_set([4, 1, 5, 1, 2, 5]), vec![1, 2, 4, 5]);
        assert_eq!(tier_set([]), Vec::<u8>::new());
    }

    #[test]
    fn segment_row_tiers_maps_segments_and_leaves_gaps_none() {
        let segs = [seg(10, 12, 0), seg(14, 15, 3)];
        assert_eq!(
            segment_row_tiers(&segs, 10, 6),
            vec![Some(0), Some(0), Some(0), None, Some(3), Some(3)]
        );
        assert_eq!(segment_row_tiers(&[], 10, 3), vec![None, None, None]);
    }

    #[test]
    fn visible_rows_subtract_fold_runs_and_never_underflows() {
        // 180-row hunk tiled rows 1-140 P5 / 141-158 P1 / 159-180 P5
        // (the segments fixture): filtering to P1 keeps the P1 band plus
        // ±3 context → 18 + 6 visible of 180.
        let segs = [seg(1, 140, 5), seg(141, 158, 1), seg(159, 180, 5)];
        assert_eq!(visible_row_estimate(&segs, 1, 180, 180, (0, 0), 1), 24);
        assert_eq!(visible_row_estimate(&segs, 1, 180, 180, (0, 0), 7), 180);
        // Single (or no) segment: nothing can fold.
        assert_eq!(
            visible_row_estimate(&[seg(1, 20, 0)], 1, 20, 20, (0, 0), 0),
            20
        );
        assert_eq!(visible_row_estimate(&[], 1, 20, 20, (0, 0), 0), 20);
        // Degenerate range (end before start): untouched.
        assert_eq!(visible_row_estimate(&segs, 5, 4, 7, (0, 0), 0), 7);
        // Everything filtered out folds all rows but never underflows.
        assert_eq!(
            visible_row_estimate(&[seg(1, 20, 5)], 1, 20, 5, (0, 0), 0),
            0
        );
    }

    /// Demo's first raw hunk: rows 6-185 → P5 head, P1 lookup, P5 tail.
    fn demo_row_tiers() -> Vec<Option<u8>> {
        let mut t = vec![Some(5); 140];
        t.extend(vec![Some(1); 18]);
        t.extend(vec![Some(5); 22]);
        t
    }

    #[test]
    fn demo_hunk_at_p1_keeps_the_band_plus_context_and_folds_the_rest() {
        let t = demo_row_tiers();
        assert_eq!(
            fold_runs(&t, 1, FOLD_CONTEXT_ROWS),
            vec![(0, 136), (161, 179)]
        );
        assert_eq!(
            seam_plan(&t, &[false; 180], 1, FOLD_CONTEXT_ROWS),
            vec![
                SeamPlan { start: 0, end: 136 },
                SeamPlan {
                    start: 161,
                    end: 179
                }
            ]
        );
    }

    #[test]
    fn demo_hunk_at_p5_folds_nothing() {
        let t = demo_row_tiers();
        assert!(fold_runs(&t, 5, FOLD_CONTEXT_ROWS).is_empty());
        assert!(seam_plan(&t, &[false; 180], 5, FOLD_CONTEXT_ROWS).is_empty());
    }

    #[test]
    fn hunk_own_context_rows_survive_the_fold() {
        // 3 leading ctx rows + P5 + P1 band + P5 + 3 trailing ctx rows,
        // the ctx rows tiered by their enclosing P5 segments.
        let mut t = vec![Some(5); 30];
        t.extend(vec![Some(1); 5]);
        t.extend(vec![Some(5); 30]);
        let mut pinned = vec![false; t.len()];
        pin_context(&mut pinned, 3, 3);
        assert_eq!(
            seam_plan(&t, &pinned, 1, FOLD_CONTEXT_ROWS),
            vec![
                SeamPlan { start: 3, end: 26 },
                SeamPlan { start: 38, end: 61 }
            ]
        );
        // Without the pins the leading/trailing context would fold.
        assert_eq!(
            seam_plan(&t, &vec![false; t.len()], 1, FOLD_CONTEXT_ROWS)[0],
            SeamPlan { start: 0, end: 26 }
        );
        // The height estimate agrees with the plan: 65 rows - 24 - 24.
        let segs = [seg(1, 30, 5), seg(31, 35, 1), seg(36, 65, 5)];
        assert_eq!(visible_row_estimate(&segs, 1, 65, 65, (3, 3), 1), 17);
        assert_eq!(visible_row_estimate(&segs, 1, 65, 65, (3, 3), 5), 65);
    }

    #[test]
    fn pin_context_clamps_to_the_body() {
        let mut p = vec![false; 4];
        pin_context(&mut p, 9, 0);
        assert_eq!(p, vec![true; 4]);
        let mut p = vec![false; 5];
        pin_context(&mut p, 1, 2);
        assert_eq!(p, vec![true, false, false, true, true]);
    }
}
