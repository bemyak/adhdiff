//! One `<article class="hunk">` in the detail pane: head (tier, title, loc,
//! eye/open/chevron buttons), optional rationale, the pre-highlighted
//! diff body injected via `inner_html`, and the comment section under it.
//!
//! The hunk payloads live in the `map` signal; each article reads through
//! a per-hunk Memo so a single-file comment merge re-renders only the
//! touched articles (same <For> key → DOM node kept → scroll survives).

use adhdiff_shared::{Confidence, SavedComment};
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::composer;
use crate::icons;
use crate::state::{AppState, advance_after_viewed, hunk_identity, visible_row_estimate};
use crate::{defer_scroll_focus_into_view, open_in_editor, toggle_viewed, web_document};

#[component]
pub fn HunkSection(state: AppState, oi: usize) -> impl IntoView {
    let hunk = Memo::new(move |_| state.map.with(|m| m.hunks[oi].clone()));
    let h = hunk.get_untracked();
    let tier = h.priority.as_u8();
    let map_idx = h.map_idx;
    let identity = hunk_identity(&h.file, h.row_start);
    let viewed_title = {
        let identity = identity.clone();
        move || {
            if state.viewed.with(|v| v.contains(&identity)) {
                "mark as unviewed"
            } else {
                "mark as viewed"
            }
        }
    };
    let is_viewed = move || state.viewed.with(|v| v.contains(&identity));
    let is_viewed_label = is_viewed.clone();
    let can_open = state.map.with_untracked(|m| m.editor.is_some()) && h.openable;
    let confidence = (!matches!(h.confidence, Confidence::High))
        .then(|| format!("{:?}", h.confidence).to_lowercase());
    let rationale = h.rationale.clone();
    let body_rows = h.body_rows;
    let segments = h.segments.clone();
    let (row_start, row_end) = (h.row_start, h.row_end);
    let loc = format!("{} rows {}–{}", h.file, h.row_start, h.row_end);

    view! {
        <article
            id=format!("hunk-{oi}")
            class=format!("hunk tier-p{tier}-bg-deemph")
            class:focused=move || state.focused.get() == oi
            class:collapsed=move || state.collapsed.with(|c| c.contains(&map_idx))
            aria-current=move || (state.focused.get() == oi).to_string()
            on:click=move |ev| article_click(state, oi, &ev)
        >
            <header class="hunk-head">
                <span class=format!("tier-tag tier-p{tier}")>"P" {tier}</span>
                <span class="hunk-title">{h.label.clone()}</span>
                <span class="hunk-loc">{loc}</span>
                {confidence.map(|c| view! {
                    <span class="hunk-conf" title="agent self-confidence">"(" {c} ")"</span>
                })}
                <button
                    class="view-btn"
                    class:viewed=is_viewed.clone()
                    title=viewed_title
                    aria-label=move || {
                        if is_viewed_label() {
                            "Mark as unviewed"
                        } else {
                            "Mark as viewed"
                        }
                    }
                    aria-pressed=move || is_viewed().to_string()
                    on:click=move |ev| {
                        ev.stop_propagation();
                        if toggle_viewed(state, oi)
                            && let Some(t) =
                                advance_after_viewed(&state.visible.get_untracked(), oi)
                            {
                                state.click_focus(t);
                            }
                    }
                >
                    <span class="icon-open" inner_html=icons::EYE></span>
                    <span class="icon-closed" inner_html=icons::EYE_OFF></span>
                </button>
                {can_open.then(|| view! {
                    <button
                        class="open-btn"
                        title="open in editor (e)"
                        aria-label="Open file in editor (e)"
                        on:click=move |ev| {
                            ev.stop_propagation();
                            open_in_editor(state, oi);
                        }
                        inner_html=icons::EXTERNAL
                    ></button>
                })}
                <button
                    class="chevron"
                    title="collapse/expand"
                    aria-label=move || {
                        if state.collapsed.with(|c| c.contains(&map_idx)) {
                            "Expand hunk"
                        } else {
                            "Collapse hunk"
                        }
                    }
                    aria-expanded=move || (!state.collapsed.with(|c| c.contains(&map_idx))).to_string()
                    aria-controls=format!("hunk-{oi}-body")
                    on:click=move |ev| {
                        ev.stop_propagation();
                        let mut folded = false;
                        state.collapsed.update(|c| {
                            if !c.remove(&map_idx) {
                                c.insert(map_idx);
                                folded = true;
                            }
                        });
                        state.clear_failed(oi);
                        if folded {
                            defer_scroll_focus_into_view(oi);
                        }
                    }
                    inner_html=icons::CHEVRON
                ></button>
            </header>
            {(!rationale.is_empty()).then(|| view! { <p class="rationale">{rationale}</p> })}
            // Bodies arrive lazily (see lib.rs prefetch_bodies). The slot
            // reserves the ACTIVE FILTER's row count × the stylesheet's
            // 12px × 1.55 row: rows under folded segments stop occupying
            // space once fold.rs hides them, so the reservation tracks
            // state.tier — real rows can't wrap, so the reserved height
            // IS the final block height: the inject is
            // geometry-preserving (no jump, untouched wheel anchors), and
            // content-visibility:auto keeps offscreen hunk bodies out of
            // layout/paint entirely — O(viewport) render cost regardless
            // of how many bodies stay resident.
            <div
                id=format!("hunk-{oi}-body")
                class="body-slot"
                style=move || {
                    let rows = visible_row_estimate(
                        &segments, row_start, row_end, body_rows, state.tier.get(),
                    );
                    format!(
                        "content-visibility:auto;\
                         contain-intrinsic-block-size:{h:.1}px;min-height:{h:.1}px",
                        h = f64::from(rows) * 12.0 * 1.55
                    )
                }
            >{move || match state.bodies.with(|b| b.get(&map_idx).cloned()) {
                Some(html) => view! { <div inner_html=html></div> }.into_any(),
                None => view! {
                    <div class="body-loading">"loading diff body…"</div>
                }.into_any(),
            }}</div>
            {move || {
                let comments = hunk.with(|h| h.comments.clone());
                (!comments.is_empty()).then(|| view! {
                    <section class="comments">
                        {comments.into_iter().map(|c| view! {
                            <CommentRow state=state oi=oi map_idx=map_idx c=c/>
                        }).collect_view()}
                    </section>
                })
            }}
        </article>
    }
}

/// Hunk clicks focus; gutter presses are comment gestures owned by the
/// pointer handlers, so they must not also become focus gestures. (The
/// gutter row itself is the target within the 50px number column.)
fn article_click(state: AppState, oi: usize, ev: &web_sys::MouseEvent) {
    if let Some(target) = ev
        .target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        && let Ok(Some(row)) = target.closest(".hunk-body [data-dline]")
    {
        let row_node: &web_sys::Node = row.unchecked_ref();
        if target.is_same_node(Some(row_node)) && ev.offset_x() <= 50 {
            return;
        }
    }
    state.click_focus(oi);
}

/// One stored comment under a hunk body: click re-opens the composer on
/// the comment's span (adopting its body for in-place overwrite); the ×
/// deletes it.
#[component]
fn CommentRow(state: AppState, oi: usize, map_idx: usize, c: SavedComment) -> impl IntoView {
    let loc = match c.end_row {
        Some(e) if e > c.row => format!("{} row {}–{}", c.file, c.row, e),
        _ => format!("{} row {}", c.file, c.row),
    };
    let mut lines = c.body.lines();
    let first = lines.next().unwrap_or("").to_string();
    let multi = lines.next().is_some();
    let full_body = c.body.clone();
    let file = c.file.clone();
    let start = c.row;
    view! {
        <div
            class="comment"
            data-cfile=file.clone()
            data-cstart=start
            data-cend=c.end_row.unwrap_or(c.row)
            data-oidx=oi
            role="button"
            tabindex="0"
            on:click={
                let file = file.clone();
                move |ev: web_sys::MouseEvent| {
                    ev.stop_propagation();
                    reopen_comment(state, oi, map_idx, file.clone(), start);
                }
            }
            on:keydown={
                let file = file.clone();
                move |ev: web_sys::KeyboardEvent| {
                    // role=button contract: Enter/Space activate. Space's
                    // default (scroll) must not fire.
                    if ev.key() == "Enter" || ev.key() == " " {
                        ev.prevent_default();
                        reopen_comment(state, oi, map_idx, file.clone(), start);
                    }
                }
            }
        >
            <button
                class="comment-x"
                data-action="delete-comment"
                title="delete comment"
                aria-label="Delete comment"
                on:click={
                    let file = file.clone();
                    move |ev: web_sys::MouseEvent| {
                        ev.stop_propagation();
                        composer::delete_comment(state, file.clone(), start);
                    }
                }
            >"×"</button>
            <span class="loc">{loc}</span>
            <span class="body" title=full_body>{first}{multi.then_some("…")}</span>
        </div>
    }
}

/// Re-open the composer on a stored comment's span (adopting its body for
/// in-place overwrite), expanding the hunk first when folded. Shared by the
/// comment row's click and its role=button Enter/Space activation.
fn reopen_comment(state: AppState, oi: usize, map_idx: usize, file: String, start: u32) {
    state.collapsed.update(|col| {
        col.remove(&map_idx);
    });
    let anchor_dline = comment_scroll_and_anchor(oi, start);
    composer::open_composer(state, oi, file, start, None, anchor_dline);
}

/// Center the comment's start row in the pane (smooth, like the old row
/// click) and return its dline so the composer anchors under it.
fn comment_scroll_and_anchor(oi: usize, start: u32) -> Option<u32> {
    let document = web_document()?;
    let row = document
        .query_selector(&format!("#hunk-{oi} [data-row='{start}']"))
        .ok()
        .flatten()?;
    let dline = row.get_attribute("data-dline").and_then(|d| d.parse().ok());
    let opts = web_sys::ScrollIntoViewOptions::new();
    opts.set_block(web_sys::ScrollLogicalPosition::Center);
    opts.set_behavior(if crate::reduced_motion() {
        web_sys::ScrollBehavior::Auto
    } else {
        web_sys::ScrollBehavior::Smooth
    });
    row.scroll_into_view_with_scroll_into_view_options(&opts);
    dline
}
