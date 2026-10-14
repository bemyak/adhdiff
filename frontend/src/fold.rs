//! Stage-2 segment filtering inside surviving hunks — the fine half of
//! the spec's "Filtering behavior": each loaded body folds the rows of
//! segments sitting above the active tier filter, keeping
//! [`FOLD_CONTEXT_ROWS`] around survivors and marking every folded run
//! with a seam. A row carrying a comment never folds: the run splits
//! around it and it stays visible alone, no added context. This module
//! also owns the segment affordances that hang
//! off the baked row data: the hover tooltip (`data-seg` → the item's
//! `segments` entry).
//!
//! Everything here is a DOM pass over the injected `.hunk-body` markup;
//! the run math itself is pure and lives in `state` for host tests.

use std::cell::Cell;
use std::rc::Rc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::state::{AppState, FOLD_CONTEXT_ROWS, seam_plan};
use crate::{SCROLL_IDLE_MS, article_of, event_element, web_document};

/// Wire the fold pass and the segment-hover affordances to `document`.
/// Registered once from the shell's install effect (same discipline as
/// the other `install_*` handlers). `scroll_active` is the pane's scroll
/// burst window (install_scroll, first event through idle debounce):
/// while it holds, every hover path below short-circuits — hover-driven
/// writes mid-glide force a main-thread sync that aborts the native
/// momentum scroll.
pub fn install(state: AppState, document: &web_sys::Document, scroll_active: Rc<Cell<bool>>) {
    let tooltip = document.create_element("div").ok().inspect(|el| {
        el.set_class_name("seg-tooltip");
        let _ = el.set_attribute("hidden", "");
        if let Some(body) = document.body() {
            let _ = body.append_child(el);
        }
    });
    let Some(tooltip) = tooltip else { return };

    // Last tracked pointer position (viewport coords), updated by the
    // mousemove tracker below. The scroll-settle and body-refresh
    // re-evals re-decide what's under a STATIONARY pointer from it;
    // `None` until the first move — before that there is no interest
    // to re-confirm anyway.
    let pointer: Rc<Cell<Option<(f64, f64)>>> = Rc::new(Cell::new(None));

    // The paint state, keyed by the segment ROW that produced it
    // (article index, row's data-dline); `None` = torn down. Every path
    // that paints or hides goes through this key: mouseover fires only on
    // target crossings, but the tier-cell verdict can flip WITHOUT one
    // (a lateral move inside a row — the gutter is the row's ::before,
    // the stripe its ::after, both hit-testing to the row itself), so the
    // mousemove tracker re-runs the pipeline and this key keeps
    // non-transition moves write-free.
    let painted: Rc<Cell<Option<(usize, u32)>>> = Rc::new(Cell::new(None));

    let over = {
        let tooltip = tooltip.clone();
        let painted = Rc::clone(&painted);
        let scroll_active = Rc::clone(&scroll_active);
        Closure::wrap(Box::new(move |ev: web_sys::MouseEvent| {
            if !scroll_active.get() {
                hover_over(state, &tooltip, &painted, &ev);
            }
        }) as Box<dyn FnMut(_)>)
    };
    let _ = document.add_event_listener_with_callback("mouseover", over.as_ref().unchecked_ref());
    over.forget();

    let out = {
        let tooltip = tooltip.clone();
        let painted = Rc::clone(&painted);
        let scroll_active = Rc::clone(&scroll_active);
        Closure::wrap(Box::new(move |ev: web_sys::MouseEvent| {
            if !scroll_active.get() {
                hover_out(&tooltip, &painted, &ev);
            }
        }) as Box<dyn FnMut(_)>)
    };
    let _ = document.add_event_listener_with_callback("mouseout", out.as_ref().unchecked_ref());
    out.forget();

    // The stripe gate lives in the mousemove path, not just mouseover:
    // entering a row laterally (gutter ::before → stripe ::after, same
    // target) fires no mouseover, so a mouseover-only gate only re-ran on
    // the user's "wiggle" — a micro vertical move that crossed a row
    // boundary. Here every move re-decides; the painted key makes
    // no-change moves write-free. During a scroll burst hover work stays
    // skipped (the position is still tracked for the settle re-eval).
    let glide = Closure::wrap(Box::new({
        let tooltip = tooltip.clone();
        let painted = Rc::clone(&painted);
        let pointer = Rc::clone(&pointer);
        let scroll_active = Rc::clone(&scroll_active);
        move |ev: web_sys::MouseEvent| {
            pointer.set(Some((f64::from(ev.client_x()), f64::from(ev.client_y()))));
            if scroll_active.get() {
                return;
            }
            hover_over(state, &tooltip, &painted, &ev);
            if !tooltip.has_attribute("hidden") {
                position_tooltip(&tooltip, ev.client_x(), ev.client_y());
            }
        }
    }) as Box<dyn FnMut(_)>);
    let _ = document.add_event_listener_with_callback("mousemove", glide.as_ref().unchecked_ref());
    glide.forget();

    // A pane scroll makes the affordances under the parked pointer stale
    // within a frame: hide both at burst start. Once hidden the guard
    // short-circuits, so a long glide pays at most one hide.
    let scroll_hide = Closure::wrap(Box::new({
        let tooltip = tooltip.clone();
        let painted = Rc::clone(&painted);
        move |ev: web_sys::Event| {
            if painted.get().is_none() {
                return;
            }
            let in_detail = ev
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                .and_then(|el| el.closest(".detail").ok().flatten())
                .is_some();
            if in_detail {
                hide_segment_ui(&tooltip, &painted);
            }
        }
    }) as Box<dyn FnMut(_)>);
    let opts = web_sys::AddEventListenerOptions::new();
    opts.set_capture(true);
    opts.set_passive(true);
    let _ = document.add_event_listener_with_callback_and_add_event_listener_options(
        "scroll",
        scroll_hide.as_ref().unchecked_ref(),
        &opts,
    );
    scroll_hide.forget();

    // Scroll-settle re-eval, an independent debounce on the same idle
    // window install_scroll uses (own timer, no shared handle): a burst
    // moves the content under a parked pointer — scroll_hide tore the
    // affordances down, but the element there never changed from the
    // DOM's view, so neither mouseover nor (with the pointer stationary)
    // mousemove fires. Without this the tooltip stayed hidden until a
    // wiggle. Fires only after quiet, so the glide stays hover-free.
    let settle_timer = Rc::new(Cell::new(-1_i32));
    let settle: Closure<dyn FnMut()> = Closure::wrap(Box::new({
        let settle_timer = Rc::clone(&settle_timer);
        let tooltip = tooltip.clone();
        let painted = Rc::clone(&painted);
        let pointer = Rc::clone(&pointer);
        move || {
            settle_timer.set(-1);
            if let Some((x, y)) = pointer.get() {
                evaluate_hovers(state, &tooltip, &painted, x, y);
            }
        }
    }));
    let settle_fn: js_sys::Function = settle.as_ref().unchecked_ref::<js_sys::Function>().clone();
    let settle_watch = Closure::wrap(Box::new(move |ev: web_sys::Event| {
        let in_detail = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|el| el.closest(".detail").ok().flatten())
            .is_some();
        if !in_detail {
            return;
        }
        let Some(window) = web_sys::window() else {
            return;
        };
        let prev = settle_timer.replace(-1);
        if prev >= 0 {
            window.clear_timeout_with_handle(prev);
        }
        if let Ok(id) =
            window.set_timeout_with_callback_and_timeout_and_arguments_0(&settle_fn, SCROLL_IDLE_MS)
        {
            settle_timer.set(id);
        }
    }) as Box<dyn FnMut(_)>);
    let _ = document.add_event_listener_with_callback_and_add_event_listener_options(
        "scroll",
        settle_watch.as_ref().unchecked_ref(),
        &opts,
    );
    settle_watch.forget();
    settle.forget();

    // Re-plan folds when a body lands (the inject wiped any prior seams),
    // when articles re-mount (re-injected bodies arrive unfolded) and
    // whenever the tier filter moves — then re-decide the affordances
    // under a parked pointer: a landing body bakes `data-seg` rows where
    // the hover started on provisional markup, and the pointer fires no
    // fresh mouseover for content that appears beneath it. Effects run
    // after the render flush, so the just-injected body is in the DOM.
    // Mid-burst the re-eval belongs to the settle timer above.
    Effect::new(move |_| {
        state.bodies.track();
        state.visible.track();
        let max_tier = state.tier.get();
        refold_all(state, max_tier);
        if !scroll_active.get()
            && let Some((x, y)) = pointer.get()
        {
            evaluate_hovers(state, &tooltip, &painted, x, y);
        }
    });
}

/// Re-plan every hunks's loaded body against the current filter.
fn refold_all(state: AppState, max_tier: u8) {
    let Some(document) = web_document() else {
        return;
    };
    let Ok(articles) = document.query_selector_all(".hunk") else {
        return;
    };
    for i in 0..articles.length() {
        let Some(node) = articles.item(i) else {
            continue;
        };
        let art: web_sys::Element = node.unchecked_into();
        let Some(oi) = art
            .id()
            .strip_prefix("hunk-")
            .and_then(|s| s.parse::<usize>().ok())
        else {
            continue;
        };
        refold_hunk(state, max_tier, &art, oi);
    }
}

/// One body's fold pass. Idempotent: the previous pass's seams and hides
/// are stripped before re-planning (bodies are also re-injected wholesale
/// on comment merges; tier changes re-plan over the same live rows).
fn refold_hunk(state: AppState, max_tier: u8, art: &web_sys::Element, oi: usize) {
    if let Ok(seams) = art.query_selector_all(".fold-seam") {
        for i in 0..seams.length() {
            if let Some(node) = seams.item(i) {
                let el: web_sys::Element = node.unchecked_into();
                el.remove();
            }
        }
    }
    if let Ok(hidden) = art.query_selector_all(".fold-hidden") {
        for i in 0..hidden.length() {
            if let Some(node) = hidden.item(i) {
                let el: web_sys::Element = node.unchecked_into();
                let _ = el.class_list().remove_1("fold-hidden");
            }
        }
    }
    // Degenerate tilings can't fold; legacy maps have no segments at all.
    let seg_count = state
        .map
        .with_untracked(|m| m.hunks.get(oi).map(|h| h.segments.len()))
        .unwrap_or(0);
    if seg_count <= 1 {
        return;
    }
    let Ok(rows) = art.query_selector_all(".hunk-body div[data-dline]") else {
        return;
    };
    let mut tiers = Vec::with_capacity(rows.length() as usize);
    let mut commented = Vec::with_capacity(rows.length() as usize);
    for i in 0..rows.length() {
        let Some(node) = rows.item(i) else { continue };
        let el: web_sys::Element = node.unchecked_into();
        tiers.push(row_tier(&el));
        commented.push(el.class_list().contains("line-commented"));
    }
    let Some(document) = web_document() else {
        return;
    };
    for plan in seam_plan(&tiers, &commented, max_tier, FOLD_CONTEXT_ROWS) {
        for i in plan.start..=plan.end {
            if let Some(node) = rows.item(i as u32) {
                let el: web_sys::Element = node.unchecked_into();
                let _ = el.class_list().add_1("fold-hidden");
            }
        }
        let Some(first) = rows.item(plan.start as u32) else {
            continue;
        };
        let first: web_sys::Element = first.unchecked_into();
        let Ok(seam) = document.create_element("div") else {
            continue;
        };
        seam.set_class_name("fold-seam");
        let n = plan.end - plan.start + 1;
        seam.set_text_content(Some(&format!("∇ {n} lines folded")));
        let _ = first.before_with_node_1(&seam);
    }
}

/// The row's segment tier from its baked `seg-p{N}` class; `None` when
/// no segment claims the row (never folds).
fn row_tier(el: &web_sys::Element) -> Option<u8> {
    let classes = el.class_list();
    for i in 0..classes.length() {
        if let Some(class) = classes.item(i)
            && let Some(rest) = class.strip_prefix("seg-p")
        {
            return rest.parse::<u8>().ok();
        }
    }
    None
}

/// Width of the tier cell (::after) at the far left of segmented rows —
/// tooltip triggers only when the pointer is inside it, so
/// reading code mid-hunk never pops segment chrome. Mirrors styles.css
/// (width: 11px).
const TIER_CELL_PX: f64 = 11.0;

/// Tier cell hovered: paint the segment tooltip at the cursor — and, when
/// the hovered hunk holds focus, the segment label into its head line.
/// Any hover NOT inside a tier cell (code area, number gutter, wrong
/// hunk, degenerate tiling, non-segment element) tears down both instead
/// of going stale. The live-event shim: mouseover AND the mousemove
/// tracker both land here (the tracker is what catches lateral moves into
/// the cell that never retarget).
fn hover_over(
    state: AppState,
    tooltip: &web_sys::Element,
    painted: &Cell<Option<(usize, u32)>>,
    ev: &web_sys::MouseEvent,
) {
    hover_hit(
        state,
        tooltip,
        painted,
        event_element(ev.as_ref()),
        ev.client_x(),
        ev.client_y(),
    );
}

/// Post-hoc re-eval under a STATIONARY pointer: the same displayability
/// pipeline as [`hover_over`], but the hit is sourced from the point
/// under the last tracked pointer position instead of an event target.
/// Scroll settles and body refreshes change what's under the pointer
/// without firing a mouseover or a mousemove; this is their interest
/// re-check. The tooltip itself is `pointer-events:none` + `[hidden]`,
/// so `element_from_point` never returns it as the hit.
fn evaluate_hovers(
    state: AppState,
    tooltip: &web_sys::Element,
    painted: &Cell<Option<(usize, u32)>>,
    client_x: f64,
    client_y: f64,
) {
    let hit = web_document().and_then(|d| d.element_from_point(client_x as f32, client_y as f32));
    hover_hit(
        state,
        tooltip,
        painted,
        hit,
        client_x as i32,
        client_y as i32,
    );
}

/// The shared displayability pipeline. `hit` is the element at
/// (client_x, client_y), however sourced. Writes ONLY when the decision
/// changed: the `painted` key (article index + the row's `data-dline`)
/// mirrors what both affordances currently show, so mousemove-frequency
/// calls with a stable verdict stay write-free — no text/attribute writes
/// means no layout invalidation per move.
fn hover_hit(
    state: AppState,
    tooltip: &web_sys::Element,
    painted: &Cell<Option<(usize, u32)>>,
    hit: Option<web_sys::Element>,
    client_x: i32,
    client_y: i32,
) {
    let found = hit
        .and_then(|el| el.closest(".hunk-body [data-seg]").ok().flatten())
        .and_then(|row| {
            let rel_x = f64::from(client_x) - row.get_bounding_client_rect().left();
            (rel_x <= TIER_CELL_PX).then_some(row)
        })
        .and_then(|row| {
            let (_, oi) = article_of(&row)?;
            let dline = row.get_attribute("data-dline")?.parse::<u32>().ok()?;
            let seg_idx = row.get_attribute("data-seg")?.parse::<usize>().ok()?;
            let seg = state.map.with_untracked(|m| {
                let h = m.hunks.get(oi)?;
                (h.segments.len() > 1)
                    .then(|| h.segments.get(seg_idx).cloned())
                    .flatten()
            })?;
            Some((oi, dline, seg))
        });
    let key = found.as_ref().map(|(oi, dline, _)| (*oi, *dline));
    if key == painted.get() {
        return;
    }
    let Some((oi, dline, seg)) = found else {
        hide_segment_ui(tooltip, painted);
        return;
    };
    painted.set(Some((oi, dline)));
    tooltip.set_text_content(Some(&if seg.rationale.is_empty() {
        format!("P{} {}", seg.tier, seg.label)
    } else {
        format!("P{} {} — {}", seg.tier, seg.label, seg.rationale)
    }));
    let _ = tooltip.remove_attribute("hidden");
    position_tooltip(tooltip, client_x, client_y);
}

/// Fires after the destination's mouseover (over-then-out order), so the
/// pointer landing on another body row has ALREADY re-decided — hide only
/// when it left the body's rows entirely (gutter gap aside: every row
/// carries data-dline, so a None closest means outline/head/seam/void).
fn hover_out(
    tooltip: &web_sys::Element,
    painted: &Cell<Option<(usize, u32)>>,
    ev: &web_sys::MouseEvent,
) {
    let from_seg_row = event_element(ev.as_ref())
        .and_then(|el| el.closest(".hunk-body [data-seg]").ok().flatten())
        .is_some();
    if !from_seg_row {
        return;
    }
    let to_body_row = ev
        .related_target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        .and_then(|el| el.closest(".hunk-body [data-dline]").ok().flatten())
        .is_some();
    if to_body_row {
        return;
    }
    hide_segment_ui(tooltip, painted);
}

/// Tooltip back to hidden, paint key cleared
/// (every teardown clears it — a stale key would veto the very next
/// re-decision over the same row, see hover_hit's early return).
fn hide_segment_ui(tooltip: &web_sys::Element, painted: &Cell<Option<(usize, u32)>>) {
    let _ = tooltip.set_attribute("hidden", "");
    painted.set(None);
}

/// Fixed-position tooltip just off the cursor, kept inside the viewport
/// once its true width is known (hidden measurings read 0 wide — clamp
/// against the generous CSS max-width then).
fn position_tooltip(tooltip: &web_sys::Element, x: i32, y: i32) {
    let mut left = x + 12;
    let top = y + 16;
    if let Some(window) = web_sys::window()
        && let Ok(inner) = window.inner_width()
        && let Some(iw) = inner.as_f64()
    {
        let width = f64::from(
            tooltip
                .dyn_ref::<web_sys::HtmlElement>()
                .map_or(420, |el| el.offset_width().max(1)),
        );
        let max_left = (iw as i32 - width as i32 - 8).max(8);
        left = left.min(max_left);
    }
    let _ = tooltip.set_attribute("style", &format!("left:{left}px;top:{top}px"));
}
