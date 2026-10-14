//! Left outline sidebar: the filter bar (panel toggle, tier switch) and one
//! row per visible hunk with tier/file/label plus comment and viewed marks.

use leptos::prelude::*;

use crate::icons;
use crate::state::{AppState, hunk_identity};

#[component]
pub fn FilterBar(state: AppState) -> impl IntoView {
    let tiers = state.tiers.get_untracked();
    view! {
        <div class="outline-filter">
            <button
                class="panel-toggle"
                title="toggle outline (f)"
                aria-label="Toggle outline (f)"
                aria-expanded=move || state.outline_open.get().to_string()
                aria-controls="outline-panel"
                on:click=move |_| state.outline_open.update(|o| *o = !*o)
                inner_html=icons::PANEL_LEFT
            ></button>
            <div class="filter-group">
                <button
                    class="panel-toggle"
                    class:on=move || state.comments_only.get()
                    title="Show only hunks with comments"
                    aria-label="Show only hunks with comments"
                    aria-pressed=move || state.comments_only.get().to_string()
                    on:click=move |_| {
                        state.comments_only.update(|c| *c = !*c);
                        state.say(if state.comments_only.get_untracked() {
                            "Showing only hunks with comments".to_string()
                        } else {
                            "Showing all hunks".to_string()
                        });
                    }
                    inner_html=icons::COMMENT
                ></button>
                <div class="mode-switch tier-filter">
                    {tiers
                        .into_iter()
                        .map(|t| view! {
                            <button
                                data-maxtier=t
                                class:active=move || state.tier.get() == t
                                on:click=move |_| {
                                    state.tier.set(t);
                                    state.say(format!("Showing tiers P0 through P{t}"));
                                }
                            >
                                "P" {t}
                            </button>
                        })
                        .collect_view()}
                </div>
                <span class="filter-icon" title="Tier filter" inner_html=icons::FILTER></span>
            </div>
        </div>
    }
}

#[component]
pub fn OutlineRow(state: AppState, oi: usize) -> impl IntoView {
    let item = Memo::new(move |_| state.map.with(|m| m.outline[oi].clone()));
    let (depth, tier, file, row, label) = state.map.with_untracked(|m| {
        let item = &m.outline[oi];
        let h = &m.hunks[oi];
        let label = if item.label.is_empty() {
            "(unlabeled)".to_string()
        } else {
            item.label.clone()
        };
        (
            item.depth,
            h.priority.as_u8(),
            h.file.clone(),
            h.row_start,
            label,
        )
    });
    let identity = hunk_identity(&file, row);
    let base = if depth > 0 {
        format!(
            "outline-item outline-child depth-{} tier-p{tier}-border",
            depth.min(3)
        )
    } else {
        format!("outline-item tier-p{tier}-border")
    };
    // Child rows get the guide-column count for the stylesheet; depth is
    // fixed for a rendered row (outline rebuilds wholesale on map changes).
    let style = (depth > 0).then(|| format!("--depth:{depth}"));
    // No typed aria-level attribute exists in this tachys version; set the
    // static value imperatively once the row mounts (depth never changes
    // for a rendered row).
    let row_ref = NodeRef::<leptos::html::Div>::new();
    Effect::new(move |_| {
        if let Some(el) = row_ref.get() {
            let _ = el.set_attribute("aria-level", &(depth + 1).to_string());
        }
    });

    view! {
        <div
            class=base
            style=style
            class:viewed=move || state.viewed.with(|v| v.contains(&identity))
            class:focused=move || state.focused.get() == oi
            data-idx=oi
            node_ref=row_ref
            role="treeitem"
            aria-selected=move || (state.focused.get() == oi).to_string()
            tabindex="-1"
            // Outline clicks are jump navigation: focus, then scroll the
            // detail pane to the target (click_focus itself never
            // scrolls — in-pane clicks must not move the viewport).
            on:click=move |_| {
                state.click_focus(oi);
                crate::scroll_focus_into_view(oi);
            }
        >
            <div class="meta-row">
                <span class="tier-badge">"P" {tier}</span>
                <span class="file">{format!("{file}:{row}")}</span>
                <span class="indicators">
                    {move || item.with(|i| i.commented).then(|| view! {
                        <span class="ind ind-comment on" title="has comments">"◆"</span>
                    })}
                    <span class="ind ind-viewed" title="viewed" inner_html=icons::EYE_OFF></span>
                </span>
            </div>
            <div class="label">{label}</div>
        </div>
    }
}
