//! Comment composer: singleton card overlaid in the detail pane with its top
//! edge on the bottom of the target body row, localStorage drafts keyed per span, submit/delete with a
//! single-file merge refresh afterwards (pins are baked into body_html
//! server-side, so a repainted body must come back over the wire).

use adhdiff_shared::{FileBodies, FileComments, NewComment};
use leptos::portal::Portal;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::invoke::invoke_args;
use crate::state::{AppState, ComposerState, draft_key, find_overlap};
use crate::web_document;

/// Open the composer on `file` rows `row..end` (`end <= row` deflates to the
/// single row). An overlapping existing comment wins: its span is
/// adopted and its body prefilled, so submitting overwrites in place.
/// Otherwise any durable draft for exactly this span is restored.
pub fn open_composer(
    state: AppState,
    oi: usize,
    file: String,
    row: u32,
    end_row: Option<u32>,
    anchor_dline: Option<u32>,
) {
    let mut row = row;
    let mut end = end_row.filter(|&e| e > row).unwrap_or(row);
    let existing = state.map.with_untracked(|m| {
        find_overlap(
            m.hunks.iter().flat_map(|h| h.comments.iter()),
            &file,
            row,
            end,
        )
        .cloned()
    });
    let initial_body = match &existing {
        Some(c) => {
            row = c.row;
            end = c.end_row.unwrap_or(c.row);
            c.body.clone()
        }
        None => draft_get(&draft_key(&file, row, end)).unwrap_or_default(),
    };
    state.composer_error.set(None);
    state.composer.set(Some(ComposerState {
        oi,
        file: file.clone(),
        row,
        end_row: end,
        anchor_dline,
        draft_key: draft_key(&file, row, end),
        initial_body,
    }));
}

/// Esc / outside-press / explicit cancel: close and KEEP the draft.
pub fn close_composer(state: AppState) {
    state.composer_error.set(None);
    state.composer.set(None);
}

/// Ctrl/Cmd+Enter / primary button: an empty body deletes the existing
/// comment on this span, or discards a new one (draft cleared); otherwise
/// post, clear the draft, close, and repaint the file's hunks. Ignored
/// while an add/delete call is still in flight.
pub fn submit_comment(state: AppState) {
    if state.composer_busy.get_untracked() {
        return;
    }
    let Some(spec) = state.composer.get_untracked() else {
        return;
    };
    let body = composer_body()
        .map(|el| el.value().trim().to_string())
        .unwrap_or_default();
    if body.is_empty() {
        if comment_exists(state, &spec.file, spec.row) {
            delete_comment(state, spec.file, spec.row);
        } else {
            draft_clear(&spec.draft_key);
            close_composer(state);
        }
        return;
    }
    #[derive(serde::Serialize)]
    struct AddCommentArgs {
        c: NewComment,
    }
    let args = AddCommentArgs {
        c: NewComment {
            file: spec.file.clone(),
            row: spec.row,
            end_row: (spec.end_row > spec.row).then_some(spec.end_row),
            body,
        },
    };
    state.composer_error.set(None);
    state.composer_busy.set(true);
    leptos::task::spawn_local(async move {
        let result = invoke_args::<FileComments, _>("add_comment", &args).await;
        state.composer_busy.set(false);
        match result {
            Ok(_) => {
                draft_clear(&spec.draft_key);
                close_composer(state);
                state.say(format!("Comment added at {} row {}", spec.file, spec.row));
                if let Err(err) = refresh_file(state, spec.file.clone()).await {
                    leptos::logging::warn!("get_map after add_comment failed: {err}");
                }
            }
            Err(err) => {
                leptos::logging::warn!("add_comment failed: {err}");
                report_error(state, format!("Adding the comment failed: {err}"));
            }
        }
    });
}

fn comment_exists(state: AppState, file: &str, row: u32) -> bool {
    state.map.with_untracked(|m| {
        m.hunks
            .iter()
            .flat_map(|h| h.comments.iter())
            .any(|c| c.file == file && c.row == row)
    })
}

/// Delete button / Ctrl+Shift+Backspace: delete the comment the open
/// composer is editing; a new comment has nothing to delete.
pub fn delete_open_comment(state: AppState) {
    if let Some(spec) = state.composer.get_untracked()
        && comment_exists(state, &spec.file, spec.row)
    {
        delete_comment(state, spec.file, spec.row);
    }
}

fn report_error(state: AppState, msg: String) {
    state.say(msg.clone());
    state.composer_error.set(Some(msg));
}

/// The × on a comment row: delete, then repaint the file's hunks.
pub fn delete_comment(state: AppState, file: String, row: u32) {
    if state.composer_busy.get_untracked() {
        return;
    }
    state.composer_error.set(None);
    state.composer_busy.set(true);
    leptos::task::spawn_local(async move {
        #[derive(serde::Serialize)]
        struct DeleteCommentArgs {
            file: String,
            row: u32,
        }
        let args = DeleteCommentArgs {
            file: file.clone(),
            row,
        };
        let result = invoke_args::<FileComments, _>("delete_comment", &args).await;
        state.composer_busy.set(false);
        match result {
            Ok(_) => {
                if let Some(open) = state
                    .composer
                    .get_untracked()
                    .filter(|c| c.file == file && c.row == row)
                {
                    draft_clear(&open.draft_key);
                    close_composer(state);
                }
                state.say(format!("Comment deleted at {file} row {row}"));
                if let Err(err) = refresh_file(state, file).await {
                    leptos::logging::warn!("get_map after delete_comment failed: {err}");
                }
            }
            Err(err) => {
                leptos::logging::warn!("delete_comment failed: {err}");
                report_error(state, format!("Deleting the comment failed: {err}"));
            }
        }
    });
}

/// Re-invoke get_file_bodies and merge ONLY the affected file's slice:
/// repinned body markup into the body store, fresh comments into the
/// hunk payloads, ◆ flags into the outline rows. Articles keep their DOM
/// nodes — keyed <For> keys don't change and per-hunk Memos dedup — so
/// pane scroll and focus survive the refresh untouched. Outline entries
/// merge flag-by-flag, never wholesale.
pub async fn refresh_file(state: AppState, file: String) -> Result<(), String> {
    #[derive(serde::Serialize)]
    struct FileBodiesArgs {
        file: String,
    }
    let fresh = invoke_args::<FileBodies, _>("get_file_bodies", &FileBodiesArgs { file }).await?;
    state.bodies.update(|bs| {
        for b in &fresh.bodies {
            _ = bs.insert(b.map_idx, b.body_html.clone());
        }
    });
    state.map.update(|m| {
        for fb in &fresh.bodies {
            if let Some(slot) = m.hunks.iter_mut().find(|h| h.map_idx == fb.map_idx) {
                slot.comments = fb.comments.clone();
            }
            if let Some(item) = m.outline.get_mut(fb.idx) {
                item.commented = !fb.comments.is_empty();
            }
        }
    });
    Ok(())
}

#[component]
pub fn Composer(state: AppState, spec: ComposerState) -> impl IntoView {
    let host = insert_host(&spec);
    if let Some(host) = host.clone() {
        let host = StoredValue::new_local(host);
        let (oi, end_row) = (spec.oi, spec.end_row);
        // Re-place the overlay whenever row geometry may have moved, or close
        // (draft kept) when the hunk is filtered out.
        Effect::new(move |_| {
            state.bodies.track();
            state.tier.track();
            state.collapsed.track();
            if !state.visible.with(|v| v.contains(&oi)) {
                close_composer(state);
                return;
            }
            host.with_value(|h| position_host(h, oi, end_row));
        });
        on_cleanup(move || {
            host.try_with_value(|h| h.remove());
        });
    }
    match host {
        Some(host) => view! {
            <Portal mount=host>
                <ComposerCard state=state spec=spec.clone()/>
            </Portal>
        }
        .into_any(),
        None => view! {
            <Portal>
                <ComposerCard state=state spec=spec.clone()/>
            </Portal>
        }
        .into_any(),
    }
}

#[component]
fn ComposerCard(state: AppState, spec: ComposerState) -> impl IntoView {
    let range = if spec.end_row > spec.row {
        format!("–{}", spec.end_row)
    } else {
        String::new()
    };
    let meta = format!("hunk #{}  {} row {}{}", spec.oi, spec.file, spec.row, range);
    let draft = spec.draft_key.clone();
    let editing = comment_exists(state, &spec.file, spec.row);
    Effect::new(move |_| {
        if let Some(el) = composer_body() {
            let _ = el.focus();
        }
    });

    view! {
        <div class="composer">
            <div class="composer-card">
                <h3>"Comment"</h3>
                <div class="meta">{meta}</div>
                <textarea
                    placeholder="Markdown OK. Ctrl/Cmd+Enter saves + publishes. Empty = delete/discard."
                    on:input=move |ev| draft_set(&draft, &event_target_value(&ev))
                >{spec.initial_body.clone()}</textarea>
                {move || state.composer_error.get().map(|e| view! { <div class="error">{e}</div> })}
                <div class="buttons">
                    {editing.then(|| view! {
                        <button class="danger" aria-label="Delete comment (Ctrl+Shift+Backspace)" prop:disabled=move || state.composer_busy.get() on:click=move |_| delete_open_comment(state)>"delete"</button>
                    })}
                    <button class="secondary" aria-label="Cancel comment (draft kept)" on:click=move |_| close_composer(state)>"esc"</button>
                    <button class="primary" aria-label="Save comment" prop:disabled=move || state.composer_busy.get() on:click=move |_| submit_comment(state)>"ctrl+enter"</button>
                </div>
            </div>
        </div>
    }
}

/// Create the composer's host `<div>` in the detail pane (so neither the
/// article's overflow clip nor the body slot's paint containment cuts the
/// card) and place it. `None` when the pane isn't mounted.
fn insert_host(spec: &ComposerState) -> Option<web_sys::Element> {
    let document = web_document()?;
    let pane = document.query_selector(".detail").ok().flatten()?;
    let host = document.create_element("div").ok()?;
    host.set_class_name("composer-slot");
    pane.append_child(&host).ok()?;
    position_host(&host, spec.oi, spec.end_row);
    Some(host)
}

/// Overlay the host with its top edge on the bottom edge of the span's last
/// row, left-aligned with the code column. The host lives in the pane's
/// scroll content, so it follows the rows when the pane scrolls.
fn position_host(host: &web_sys::Element, oi: usize, end_row: u32) {
    let Some(pane) = host.parent_element() else {
        return;
    };
    let Some(document) = web_document() else {
        return;
    };
    let Some(row) = document
        .query_selector(&format!("#hunk-{oi} .hunk-body [data-row='{end_row}']"))
        .ok()
        .flatten()
    else {
        return;
    };
    let (r, p) = (
        row.get_bounding_client_rect(),
        pane.get_bounding_client_rect(),
    );
    let top = r.bottom() - p.top() + f64::from(pane.scroll_top());
    let left = r.left() - p.left() + f64::from(pane.scroll_left()) + CODE_COLUMN_PX;
    let _ = host.set_attribute("style", &format!("top:{top}px;left:{left}px"));
}

/// Left padding of a body row: where the code column starts (styles.css).
const CODE_COLUMN_PX: f64 = 50.0;

fn composer_body() -> Option<web_sys::HtmlTextAreaElement> {
    web_document()
        .and_then(|d| d.query_selector(".composer-card textarea").ok().flatten())
        .map(|el| el.unchecked_into::<web_sys::HtmlTextAreaElement>())
}

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window().and_then(|w| w.local_storage().ok().flatten())
}

fn draft_get(key: &str) -> Option<String> {
    local_storage().and_then(|s| s.get_item(key).ok().flatten())
}

fn draft_set(key: &str, value: &str) {
    if let Some(s) = local_storage() {
        let _ = s.set_item(key, value);
    }
}

fn draft_clear(key: &str) {
    if let Some(s) = local_storage() {
        let _ = s.remove_item(key);
    }
}
