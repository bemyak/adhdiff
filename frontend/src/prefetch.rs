//! Body-prefetch selection: which hunks to warm for a viewport position.
//! Pure — no DOM, no signals — so the host test suite pins the geometry;
//! `lib.rs` owns the fetch glue.

/// Outline indices of hunks whose vertical extent intersects the prefetch
/// range `[view_top - behind_px, view_bottom + ahead_px]`, ordered nearest
/// the viewport first (equal gaps keep DOM order). `heads` are (oi, top)
/// pairs in pane scroll space, ascending in DOM order; a hunk's extent runs
/// from its head to the next head, the last one to `content_end`. Matching
/// by extent (not head position) means a long hunk spanning the viewport
/// always qualifies even when its head lies far outside the range.
/// Direction bias lives in the caller's ahead/behind split.
pub fn prefetch_viewport_window(
    heads: &[(usize, f64)],
    content_end: f64,
    view_top: f64,
    view_bottom: f64,
    ahead_px: f64,
    behind_px: f64,
) -> Vec<usize> {
    let lo = view_top - behind_px.max(0.0);
    let hi = view_bottom + ahead_px.max(0.0);
    let mut hits: Vec<(f64, usize)> = Vec::with_capacity(heads.len());
    for (i, &(oi, top)) in heads.iter().enumerate() {
        let end = if i + 1 < heads.len() {
            heads[i + 1].1
        } else {
            content_end
        };
        if top >= hi || end <= lo {
            continue;
        }
        let gap = if end <= view_top {
            view_top - end
        } else if top >= view_bottom {
            top - view_bottom
        } else {
            0.0
        };
        hits.push((gap, oi));
    }
    hits.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    hits.into_iter().map(|(_, oi)| oi).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_window_empty() {
        assert_eq!(
            prefetch_viewport_window(&[], 1000.0, 0.0, 500.0, 100.0, 100.0),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn viewport_window_matches_by_extent_not_head() {
        // One long hunk whose head is far above the viewport: head-based
        // matching would miss it, but its extent spans the whole viewport.
        let heads = [(0, 0.0)];
        assert_eq!(
            prefetch_viewport_window(&heads, 10_000.0, 5000.0, 6000.0, 0.0, 0.0),
            vec![0]
        );
    }

    #[test]
    fn viewport_window_exact_viewport_only() {
        let heads = [(0, 0.0), (1, 1000.0), (2, 2000.0), (3, 3000.0)];
        // View [800, 1800]: hunk 0's extent [0,1000) overlaps it, hunk 1
        // fills it; hunks 2+ start past the bottom edge (zero margins).
        assert_eq!(
            prefetch_viewport_window(&heads, 4000.0, 800.0, 1800.0, 0.0, 0.0),
            vec![0, 1]
        );
        assert_eq!(
            prefetch_viewport_window(&heads, 4000.0, 1000.0, 2000.0, 0.0, 0.0),
            vec![1]
        );
    }

    #[test]
    fn viewport_window_margins_reach_neighbours() {
        // Hunk 2's extent [1100, 2100) spans the viewport [1150, 1450].
        let heads = [(0, 0.0), (1, 100.0), (2, 1100.0), (3, 2100.0)];
        // behind 40 stops 10px short of hunk 1's tail; ahead 600 stops
        // 50px short of hunk 3's head: only the viewport hunk qualifies.
        assert_eq!(
            prefetch_viewport_window(&heads, 2600.0, 1150.0, 1450.0, 600.0, 40.0),
            vec![2]
        );
        // Behind bias (scrolling up): the hunk above enters, hunk 3 out.
        assert_eq!(
            prefetch_viewport_window(&heads, 2600.0, 1150.0, 1450.0, 40.0, 600.0),
            vec![2, 1]
        );
        // Ahead bias (scrolling down): the hunk below enters, hunk 1 out.
        assert_eq!(
            prefetch_viewport_window(&heads, 2600.0, 1150.0, 1450.0, 900.0, 40.0),
            vec![2, 3]
        );
    }

    #[test]
    fn viewport_window_orders_nearest_first() {
        let heads = [(0, 0.0), (1, 100.0), (2, 1100.0), (3, 2100.0)];
        // Viewport inside hunk 2: it hits gap 0, hunk 1 trails 50 above,
        // hunk 3 leads 650 below — in-range first, then by distance.
        assert_eq!(
            prefetch_viewport_window(&heads, 2600.0, 1150.0, 1450.0, 900.0, 600.0),
            vec![2, 1, 3]
        );
        // The last hunk's extent runs to content_end, not infinity: a view
        // wholly past the end fetches nothing.
        assert_eq!(
            prefetch_viewport_window(&heads, 2600.0, 2650.0, 3000.0, 0.0, 0.0),
            Vec::<usize>::new()
        );
    }
}
