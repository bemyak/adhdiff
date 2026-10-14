mod composer;
mod fold;
mod hunk;
mod icons;
mod invoke;
mod outline;
mod prefetch;
mod state;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adhdiff_shared::{HunkBody, MapPayload, ViewedState};
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use state::{
    AppState, DEFAULT_ZOOM, DragState, WheelTick, advance_after_viewed, decide_wheel_focus,
    derive_focus, hunk_identity, step_commented, step_visible, zoom_in, zoom_out,
};

const KEYMAP_HELP: &str = include_str!("help.txt");

#[component]
pub fn App() -> impl IntoView {
    let map = LocalResource::new(|| async move { invoke::invoke::<MapPayload>("get_map").await });

    view! {
        {move || match map.get() {
            None => view! { <p>"Loading map…"</p> }.into_any(),
            Some(Err(err)) => view! { <p>"get_map failed: " {err}</p> }.into_any(),
            Some(Ok(map)) => view! { <ReviewApp map=map/> }.into_any(),
        }}
    }
}

#[component]
fn ReviewApp(map: MapPayload) -> impl IntoView {
    let state = AppState::new(map);
    install(state);

    let title = state.map.with_untracked(|m| m.title.clone());
    let summary = state.map.with_untracked(|m| m.summary.clone());

    view! {
        <header>
            <div class="title-block">
                <h1>{title}</h1>
                <p class="summary">{summary}</p>
            </div>
        </header>
        <div class="layout-row">
            <aside
                id="outline-panel"
                class="outline"
                class:closed=move || !state.outline_open.get()
                role="complementary"
                aria-label="Outline"
            >
                <outline::FilterBar state=state/>
                <div class="outline-rows" role="tree" aria-label="Hunks">
                    <For
                        each=move || state.visible.get()
                        key=|&oi| oi
                        children=move |oi| view! { <outline::OutlineRow state=state oi=oi/> }
                    />
                </div>
            </aside>
            <main class="detail">
                <For
                    each=move || state.visible.get()
                    key=move |&oi| state.map.with_untracked(|m| m.hunks[oi].map_idx)
                    children=move |oi| view! { <hunk::HunkSection state=state oi=oi/> }
                />
            </main>
        </div>
        {move || state.composer.get().map(|spec| view! {
            <composer::Composer state=state spec=spec/>
        })}
        {move || state.help_open.get().then(|| view! {
            <div class="overlay" role="dialog" aria-modal="true" aria-label="Keyboard shortcuts" tabindex="-1">
                <pre>{KEYMAP_HELP}</pre>
            </div>
        })}
        <div class="visually-hidden" role="status" aria-live="polite">{move || state.announce.get()}</div>
    }
}

pub(crate) fn web_document() -> Option<web_sys::Document> {
    web_sys::window().and_then(|w| w.document())
}

fn install(state: AppState) {
    let inflight = Rc::new(RefCell::new(std::collections::HashSet::new()));
    // Pane scroll offset at the last debounced rest ("scroll idle") —
    // direction source for every geometry-triggered warm; written only
    // by install_scroll's quiet-point timer.
    let rest_scroll = Rc::new(Cell::new(0.0_f64));
    // Set for the duration of a `.detail` scroll burst — first scroll
    // event through the idle debounce (see install_scroll). fold.rs gates
    // every hover path on it so parked-pointer hover churn (DOM writes,
    // forced layouts) can't sync the main thread and kill the momentum
    // glide mid-flick.
    let scroll_active = Rc::new(Cell::new(false));
    let inflight_fx = Rc::clone(&inflight);
    let rest_fx = Rc::clone(&rest_scroll);
    let scroll_fx = Rc::clone(&scroll_active);
    Effect::new(move |_| {
        if let Some(document) = web_document() {
            document.set_title(&format!("adhdiff · {}", title_of(state)));
            install_keydown(state, &document);
            install_wheel(state, &document);
            install_scroll(
                state,
                &document,
                Rc::clone(&inflight_fx),
                Rc::clone(&rest_fx),
                Rc::clone(&scroll_fx),
            );
            install_pointer(state, &document);
            fold::install(state, &document, Rc::clone(&scroll_fx));
        }
        // Bring the initial focus (first non-viewed hunk) into view after
        // the first layout — unless the head already sits in the upper
        // pane at the natural scrollTop=0 (first-hunk focus), where
        // scroll_into_view would only drag the pane down to the head's
        // scroll-margin.
        if let Some(window) = web_sys::window() {
            let init = state.focused.get_untracked();
            let cb = Closure::once_into_js(move || {
                if !head_in_upper_pane(init) {
                    scroll_focus_into_view(init);
                }
            });
            let _ = window.request_animation_frame(cb.unchecked_ref());
        }
    });
    // Live range marks: the armed drag's window while dragging, the open
    // composer's span afterwards. The effect owns .line-in-range and
    // .line-composing end to end; with neither active every mark is wiped.
    Effect::new(move |_| paint_range(state));

    // Help dialog focus discipline: stash the invoker on open and move
    // focus into the dialog (effects run after the render flush, so the
    // overlay node exists — a rAF defer would be the wrong tool here:
    // this webview starves rAF when the window lacks focus), then restore
    // the invoker on close (Esc or '?').
    let help_stash = Rc::new(RefCell::new(None::<web_sys::Element>));
    Effect::new(move |_| {
        let open = state.help_open.get();
        let Some(document) = web_document() else {
            return;
        };
        if open {
            *help_stash.borrow_mut() = document.active_element();
            if let Some(overlay) = document.query_selector(".overlay").ok().flatten() {
                let _ = overlay.unchecked_ref::<web_sys::HtmlElement>().focus();
            }
        } else if let Some(prev) = help_stash.borrow_mut().take()
            && document.contains(Some(prev.unchecked_ref::<web_sys::Node>()))
            && let Some(el) = prev.dyn_ref::<web_sys::HtmlElement>()
        {
            let _ = el.focus();
        }
    });

    // Body prefetch: fills `bodies` for the window intersecting the
    // viewport (plus direction-biased margins; focused is always
    // included). This effect NEVER writes `focused` (writer discipline
    // stays key/click/wheel); its only reactive output is `bodies`.
    Effect::new(move |_| {
        let dir = detail_pane()
            .map(|p| f64::from(p.scroll_top()) - rest_scroll.get())
            .unwrap_or(0.0);
        prefetch_bodies(state, &inflight, dir);
    });

    // Zoom applier: persists the factor and pushes it to the shell's
    // native webview zoom (wry's set_zoom_level — scales everything,
    // HiDPI included). Reader-only effect: it re-centers the focused
    // hunk once the new layout lands and never writes `focused`.
    Effect::new(move |_| {
        let factor = state.zoom.get();
        zoom_store(factor);
        leptos::task::spawn_local(async move {
            #[derive(serde::Serialize)]
            struct SetZoomArgs {
                factor: f64,
            }
            match invoke::invoke_args::<serde_json::Value, _>("set_zoom", &SetZoomArgs { factor })
                .await
            {
                Ok(_) => {
                    // Re-center the focused hunk once the new layout
                    // lands — unless its head already rests in the upper
                    // pane (fresh load on the first hunk: the startup
                    // same-factor application would only drag the pane
                    // down by the scroll-margin).
                    let cb = Closure::once_into_js(move || {
                        let oi = state.focused.get_untracked();
                        if !head_in_upper_pane(oi) {
                            scroll_focus_into_view(oi);
                        }
                    });
                    if let Some(w) = web_sys::window() {
                        let _ = w.request_animation_frame(cb.unchecked_ref());
                    }
                }
                Err(err) => leptos::logging::warn!("set_zoom failed: {err}"),
            }
        });
    });
}

/// Persist the zoom factor as a plain float string (same key the
/// constructor seeds from).
fn zoom_store(factor: f64) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = s.set_item(state::ZOOM_STORAGE_KEY, &factor.to_string());
    }
}

/// Body prefetch horizon around the currently VIEWPORT-INTERSECTING
/// hunks, in pane viewport heights. The large margin leads the scroll
/// direction (ahead while scrolling down, behind while scrolling up).
const BODY_AHEAD_SCREENS: f64 = 2.5;
const BODY_BEHIND_SCREENS: f64 = 1.0;

/// Fresh fetches one warm round may start. Each completion re-runs the
/// warm (see `fetch_body`), chaining the backfill in viewport-proximity
/// order without queueing the whole window ahead of the focused body.
const BODY_FETCH_BATCH: usize = 6;

/// Scroll quiet period after which the pane counts as at rest: fires the
/// settle prefetch that catches every motion source (wheel bursts,
/// keyboard paging, scrollbar drags). fold.rs runs its hover re-eval
/// debounce on the same window (its own listener + timer).
pub(crate) const SCROLL_IDLE_MS: i32 = 120;

/// Fetch hunk bodies for the window of hunks intersecting the pane
/// viewport plus direction-biased margins. `dir` is the pane's travel
/// since the last rest: positive = scrolling down (large margin ahead),
/// negative = up (large margin behind), zero/unknown = reading direction
/// (down).
///
/// Bodies inject rows and so change geometry, but prefetch can't fight
/// the wheel derivation: derivation stays wheel-triggered, and the tick's
/// rAF runs after all synchronous injects of the frame, so `derived`
/// always reflects the pane's true midline. The remaining hazard — a body
/// landing between wheel and rAF shifting the anchors mid-equation — is
/// neutralized by sizing each unloaded body to its final height
/// (body_rows × line-height placeholder in hunk.rs), making injects
/// geometry-preserving for every head position and scrollTop.
fn prefetch_bodies(
    state: AppState,
    inflight: &Rc<RefCell<std::collections::HashSet<usize>>>,
    dir: f64,
) {
    let focused = state.focused.get();
    // Subscription-only: visible/collapsed changes must re-run the
    // effect; selection itself derives from current pane geometry.
    let _visible = state.visible.get();
    let collapsed = state.collapsed.get();
    let failed = state.failed.get();
    let Some(pane) = detail_pane() else { return };
    let rect = pane.get_bounding_client_rect();
    let vh = rect.height();
    if vh <= 0.0 {
        return;
    }
    let focused_mi = state
        .map
        .with_untracked(|m| m.hunks.get(focused).map(|h| h.map_idx));
    // Focused first: on a cold start ONLY the focused hunk's body is
    // fetched, so boot isn't gated on warming a whole window; the
    // completion re-run then fills the window outward in batches.
    let cold_start = state.bodies.with_untracked(|b| b.is_empty())
        && focused_mi.is_some_and(|mi| {
            !collapsed.contains(&mi)
                && !failed.contains(&mi)
                && !state.bodies.with_untracked(|b| b.contains_key(&mi))
                && !inflight.borrow().contains(&mi)
        });
    if cold_start {
        if let Some(mi) = focused_mi {
            fetch_body(state, inflight, mi, dir);
        }
        return;
    }
    let st = f64::from(pane.scroll_top());
    let (ahead_px, behind_px) = if dir < 0.0 {
        (BODY_BEHIND_SCREENS * vh, BODY_AHEAD_SCREENS * vh)
    } else {
        (BODY_AHEAD_SCREENS * vh, BODY_BEHIND_SCREENS * vh)
    };
    let heads = hunk_heads(&pane);
    let mut order = prefetch::prefetch_viewport_window(
        &heads,
        f64::from(pane.scroll_height()),
        st,
        st + vh,
        ahead_px,
        behind_px,
    );
    // The focused hunk rides along even outside the window: at boot the
    // pane may not have scrolled to it yet, and keyboard/outline focus
    // can sit past the trailing margin.
    if !order.contains(&focused) {
        order.insert(0, focused);
    }
    let mut started = 0;
    for oi in order {
        if started >= BODY_FETCH_BATCH {
            break;
        }
        let Some(map_idx) = state
            .map
            .with_untracked(|m| m.hunks.get(oi).map(|h| h.map_idx))
        else {
            continue;
        };
        // Folded bodies are display:none — skip until expanded (the
        // collapsed signal is tracked, so expanding re-runs this effect).
        if collapsed.contains(&map_idx) || failed.contains(&map_idx) {
            continue;
        }
        if state.bodies.with_untracked(|b| b.contains_key(&map_idx)) {
            continue;
        }
        if inflight.borrow().contains(&map_idx) {
            continue;
        }
        fetch_body(state, inflight, map_idx, dir);
        started += 1;
    }
}

/// (outline index, head top in pane scroll space) for every hunk article
/// in the pane, in DOM order.
fn hunk_heads(pane: &web_sys::Element) -> Vec<(usize, f64)> {
    let st = f64::from(pane.scroll_top());
    let pane_top = pane.get_bounding_client_rect().top();
    let Ok(nodes) = pane.query_selector_all(".hunk") else {
        return Vec::new();
    };
    let mut heads = Vec::with_capacity(nodes.length() as usize);
    for i in 0..nodes.length() {
        let Some(node) = nodes.item(i) else { continue };
        let el: web_sys::Element = node.unchecked_into();
        let Some(oi) = el
            .id()
            .strip_prefix("hunk-")
            .and_then(|s| s.parse::<usize>().ok())
        else {
            continue;
        };
        heads.push((oi, st + el.get_bounding_client_rect().top() - pane_top));
    }
    heads
}

/// Spawn the `get_hunk_body` round-trip for one hunk, deduped through
/// `inflight`. Each completion re-runs the viewport prefetch (dedup and
/// the batch cap make re-runs cheap when the window is already full —
/// the cold start gates on exactly the first fetch landing).
fn fetch_body(
    state: AppState,
    inflight: &Rc<RefCell<std::collections::HashSet<usize>>>,
    map_idx: usize,
    dir: f64,
) {
    if !inflight.borrow_mut().insert(map_idx) {
        return;
    }
    let inflight = Rc::clone(inflight);
    leptos::task::spawn_local(async move {
        #[derive(serde::Serialize)]
        struct BodyArgs {
            #[serde(rename = "mapIdx")]
            map_idx: usize,
        }
        match invoke::invoke_args::<HunkBody, _>("get_hunk_body", &BodyArgs { map_idx }).await {
            Ok(body) => state
                .bodies
                .update(|bs| _ = bs.insert(body.map_idx, body.body_html)),
            Err(err) => {
                leptos::logging::warn!("get_hunk_body({map_idx}) failed: {err}");
                state.failed.update(|f| _ = f.insert(map_idx));
            }
        }
        inflight.borrow_mut().remove(&map_idx);
        // Fill the rest of the window once the gated first fetch lands.
        prefetch_bodies(state, &inflight, dir);
    });
}

fn title_of(state: AppState) -> String {
    state.map.with_untracked(|m| m.title.clone())
}

/// Writer #1 of `focused` (via [`AppState::key_focus`]).
fn install_keydown(state: AppState, document: &web_sys::Document) {
    let cb: Closure<dyn FnMut(web_sys::KeyboardEvent)> =
        Closure::wrap(Box::new(move |ev| handle_key(state, ev)));
    let _ = document.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
    cb.forget();
}

/// Writer #3 of `focused` (via [`AppState::wheel_focus`]). Passive wheel
/// listener; the geometry read is deferred to rAF so it sees the post-scroll
/// viewport. Scroll events never touch focus. Native scrolling consumes
/// every delta — the listener never preventDefaults.
fn install_wheel(state: AppState, document: &web_sys::Document) {
    let pending = Rc::new(Cell::new(false));
    let delta = Rc::new(Cell::new(0.0_f64));
    // `.detail` scroll offset as of the last tick; -1 = never ticked, so the
    // first tick always counts as wheel-moved. Re-stored after a
    // programmatic scroll so only real wheel motion counts.
    let pane_top = Rc::new(Cell::new(-1_i32));

    let tick: Closure<dyn FnMut()> = Closure::wrap(Box::new({
        let pending = Rc::clone(&pending);
        let delta = Rc::clone(&delta);
        let pane_top = Rc::clone(&pane_top);
        move || {
            pending.set(false);
            wheel_tick(state, delta.replace(0.0), &pane_top);
        }
    }));
    let tick_fn: js_sys::Function = tick.as_ref().unchecked_ref::<js_sys::Function>().clone();

    let on_wheel: Closure<dyn FnMut(web_sys::WheelEvent)> = Closure::wrap(Box::new({
        let pending = Rc::clone(&pending);
        let delta = Rc::clone(&delta);
        move |ev: web_sys::WheelEvent| {
            let in_detail = ev
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                .and_then(|el| el.closest(".detail").ok().flatten())
                .is_some();
            if !in_detail {
                return;
            }
            delta.set(delta.get() + ev.delta_y());
            if !pending.replace(true)
                && let Some(window) = web_sys::window()
            {
                let _ = window.request_animation_frame(&tick_fn);
            }
        }
    }));

    let opts = web_sys::AddEventListenerOptions::new();
    opts.set_passive(true);
    let _ = document.add_event_listener_with_callback_and_add_event_listener_options(
        "wheel",
        on_wheel.as_ref().unchecked_ref(),
        &opts,
    );
    on_wheel.forget();
    tick.forget();
}

/// Scroll-idle body prefetch: the settle catch-all for EVERY pane motion
/// source — wheel bursts, keyboard paging, scrollbar drags, programmatic
/// scrolls. Element scroll events don't bubble, so the listener captures
/// at the document and filters by target. After [`SCROLL_IDLE_MS`] of
/// quiet the window around the intersecting hunks is re-warmed with the
/// direction the pane actually travelled since the last rest.
/// `rest_scroll` moves only at these quiet points, so effect-triggered
/// warms mid-burst still see the burst's direction. Runs independent of
/// the pointer: a scroll landing needs no mouse wiggle.
///
/// Also owns the burst's `scroll_active` window: the FIRST event of a
/// burst sets it and brands the pane `scrolling`, the same idle debounce
/// clears both (no second timer, so the class can't outlive the window
/// that owns it). Together with the sheet's `.detail.scrolling` rule and
/// fold.rs's gates this keeps every hover path — tooltip, head label,
/// `:hover` row paints — out of the glide: hover-triggered style/DOM
/// writes force a main-thread sync that aborts WebKit's async scrolling
/// thread, which is exactly how a parked pointer stopped momentum dead.
fn install_scroll(
    state: AppState,
    document: &web_sys::Document,
    inflight: Rc<RefCell<std::collections::HashSet<usize>>>,
    rest_scroll: Rc<Cell<f64>>,
    scroll_active: Rc<Cell<bool>>,
) {
    let idle_timer = Rc::new(Cell::new(-1_i32));
    let fire: Closure<dyn FnMut()> = Closure::wrap(Box::new({
        let idle_timer = Rc::clone(&idle_timer);
        let scroll_active = Rc::clone(&scroll_active);
        move || {
            idle_timer.set(-1);
            scroll_active.set(false);
            let Some(pane) = detail_pane() else { return };
            let _ = pane.class_list().remove_1("scrolling");
            let st = f64::from(pane.scroll_top());
            let dir = st - rest_scroll.replace(st);
            prefetch_bodies(state, &inflight, dir);
        }
    }));
    let fire_fn: js_sys::Function = fire.as_ref().unchecked_ref::<js_sys::Function>().clone();

    let on_scroll: Closure<dyn FnMut(web_sys::Event)> =
        Closure::wrap(Box::new(move |ev: web_sys::Event| {
            let Some(pane) = ev
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                .and_then(|el| el.closest(".detail").ok().flatten())
            else {
                return;
            };
            // Only the burst's first event touches the class; the flag also
            // idles fold.rs's hover handlers until the debounce fires.
            if !scroll_active.replace(true) {
                let _ = pane.class_list().add_1("scrolling");
            }
            let Some(window) = web_sys::window() else {
                return;
            };
            let prev = idle_timer.replace(-1);
            if prev >= 0 {
                window.clear_timeout_with_handle(prev);
            }
            if let Ok(id) = window
                .set_timeout_with_callback_and_timeout_and_arguments_0(&fire_fn, SCROLL_IDLE_MS)
            {
                idle_timer.set(id);
            }
        }));

    let opts = web_sys::AddEventListenerOptions::new();
    opts.set_capture(true);
    opts.set_passive(true);
    let _ = document.add_event_listener_with_callback_and_add_event_listener_options(
        "scroll",
        on_scroll.as_ref().unchecked_ref(),
        &opts,
    );
    on_scroll.forget();
    fire.forget();
}

/// The scrollable detail pane.
fn detail_pane() -> Option<web_sys::Element> {
    web_document().and_then(|d| d.query_selector(".detail").ok().flatten())
}

/// Gutter click/drag → range comment, document-level so the drop can land
/// anywhere: releasing off-gutter must complete the drag with the last
/// painted window, not lose it (the old off-gutter cancel is the bug the
/// pointer listeners deliberately avoid). Brands: pointerdown arms in
/// dline space and paints live; pointermove extends the window while over
/// the same article's rows; pointerup translates the window to diff rows
/// and opens the composer.
fn install_pointer(state: AppState, document: &web_sys::Document) {
    let press: Closure<dyn FnMut(web_sys::PointerEvent)> =
        Closure::wrap(Box::new(move |ev| pointer_down(state, ev)));
    let _ =
        document.add_event_listener_with_callback("pointerdown", press.as_ref().unchecked_ref());
    press.forget();
    type PointerHandler = fn(AppState, web_sys::PointerEvent);
    for (kind, handler) in [
        ("pointermove", pointer_move as PointerHandler),
        ("pointerup", pointer_up),
    ] {
        let cb: Closure<dyn FnMut(web_sys::PointerEvent)> =
            Closure::wrap(Box::new(move |ev| handler(state, ev)));
        let _ = document.add_event_listener_with_callback(kind, cb.as_ref().unchecked_ref());
        cb.forget();
    }
}

fn event_element(ev: &web_sys::Event) -> Option<web_sys::Element> {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
}

fn row_of(el: &web_sys::Element) -> Option<web_sys::Element> {
    el.closest(".hunk-body [data-dline]").ok().flatten()
}

fn dline_of(el: &web_sys::Element) -> Option<u32> {
    el.get_attribute("data-dline").and_then(|a| a.parse().ok())
}

fn diff_row_of(el: &web_sys::Element) -> Option<u32> {
    el.get_attribute("data-row").and_then(|a| a.parse().ok())
}

fn article_of(el: &web_sys::Element) -> Option<(web_sys::Element, usize)> {
    let art = el.closest(".hunk").ok().flatten()?;
    let oi = art.id().strip_prefix("hunk-")?.parse().ok()?;
    Some((art, oi))
}

fn pointer_down(state: AppState, ev: web_sys::PointerEvent) {
    let Some(target) = event_element(ev.as_ref()) else {
        return;
    };
    // Arm the drag first — its close_only window must be captured while
    // the composer this press is about to dismiss is still open.
    if ev.button() == 0 {
        arm_drag(state, &ev, &target);
    }
    // Outside-press light dismiss; cancel keeps the draft. The press itself
    // is left alone (no prevent_default/stop_propagation) and the close is
    // deferred a turn so the pressed element still gets its click.
    let open = state.composer.get_untracked();
    if open.is_some() && target.closest(".composer-card").ok().flatten().is_none() {
        let close = Closure::once_into_js(move || {
            if state.composer.get_untracked() == open {
                composer::close_composer(state);
            }
        });
        if let Some(window) = web_sys::window() {
            let _ = window
                .set_timeout_with_callback_and_timeout_and_arguments_0(close.unchecked_ref(), 0);
        }
    }
}

/// Arm conditions (old pointerdown): left button, press lands on a body
/// row div itself (not a code span) within the 50px number gutter.
fn arm_drag(state: AppState, ev: &web_sys::PointerEvent, target: &web_sys::Element) {
    let Some(row) = row_of(target) else { return };
    {
        let row_node: &web_sys::Node = row.unchecked_ref();
        if !target.is_same_node(Some(row_node)) || ev.offset_x() > 50 {
            return;
        }
    }
    let Some((_, oi)) = article_of(&row) else {
        return;
    };
    let Some(anchor) = dline_of(&row) else { return };
    let Some(anchor_row) = diff_row_of(&row) else {
        return;
    };
    let Some(file) = state
        .map
        .with_untracked(|m| m.hunks.get(oi).map(|h| h.file.clone()))
    else {
        return;
    };
    let close_only = state.composer.with_untracked(|c| {
        c.as_ref()
            .and_then(|c| (c.oi == oi && c.file == file).then_some((c.row, c.end_row)))
    });
    state.drag.set(Some(DragState {
        oi,
        anchor_dline: anchor,
        lo: anchor,
        hi: anchor,
        anchor_row,
        lo_row: anchor_row,
        hi_row: anchor_row,
        close_only,
    }));
    ev.prevent_default(); // no text selection, no follow-up click
}

fn pointer_move(state: AppState, ev: web_sys::PointerEvent) {
    let Some(d) = state.drag.get_untracked() else {
        return;
    };
    let Some(row) = event_element(ev.as_ref()).and_then(|t| row_of(&t)) else {
        return;
    };
    let Some((_, oi)) = article_of(&row) else {
        return;
    };
    if oi != d.oi {
        return;
    }
    let (Some(dline), Some(r)) = (dline_of(&row), diff_row_of(&row)) else {
        return;
    };
    let lo = d.anchor_dline.min(dline);
    let hi = d.anchor_dline.max(dline);
    if lo != d.lo || hi != d.hi {
        state.drag.set(Some(DragState {
            lo,
            hi,
            lo_row: d.anchor_row.min(r),
            hi_row: d.anchor_row.max(r),
            ..d
        }));
    }
}

fn pointer_up(state: AppState, ev: web_sys::PointerEvent) {
    let Some(d) = state.drag.get_untracked() else {
        return;
    };
    state.drag.set(None);
    // Fold the drop row into the window when it belongs to the same
    // article — regardless of where over the row the pointer came up.
    let (mut hi, mut start, mut end) = (d.hi, d.lo_row, d.hi_row);
    if let Some(row) = event_element(ev.as_ref()).and_then(|t| row_of(&t))
        && let Some((_, oi)) = article_of(&row)
        && oi == d.oi
        && let (Some(dline), Some(r)) = (dline_of(&row), diff_row_of(&row))
    {
        hi = d.anchor_dline.max(dline);
        start = d.anchor_row.min(r);
        end = d.anchor_row.max(r);
    }
    let Some(file) = state
        .map
        .with_untracked(|m| m.hunks.get(d.oi).map(|h| h.file.clone()))
    else {
        return;
    };
    if d.close_only
        .is_some_and(|(cs, ce)| cs <= start && ce >= end)
    {
        return;
    }
    composer::open_composer(state, d.oi, file, start, Some(end), Some(hi));
}

/// Repaint the range marks: clear stale members everywhere, then mark the
/// armed drag's [lo..hi] dline window (`.line-in-range` plus the pin
/// `.line-composing`), or, with no drag armed, the open composer's diff-row
/// span (`.line-composing`). Drag/composer changes and body re-injections
/// re-run this effect.
fn paint_range(state: AppState) {
    let drag = state.drag.get();
    let composer = state.composer.get();
    state.bodies.track();
    state.visible.track();
    let Some(document) = web_document() else {
        return;
    };
    if let Ok(nodes) = document.query_selector_all(".line-in-range, .line-composing") {
        for i in 0..nodes.length() {
            if let Some(node) = nodes.item(i) {
                let el: web_sys::Element = node.unchecked_into();
                let _ = el.class_list().remove_2("line-in-range", "line-composing");
            }
        }
    }
    let (oi, lo, hi, by_row) = match (drag, composer) {
        (Some(d), _) => (d.oi, d.lo, d.hi, false),
        (None, Some(c)) => (c.oi, c.row, c.end_row, true),
        (None, None) => return,
    };
    let Some(art) = document
        .query_selector(&format!("#hunk-{oi}"))
        .ok()
        .flatten()
    else {
        return;
    };
    let Ok(rows) = art.query_selector_all(".hunk-body [data-dline]") else {
        return;
    };
    for i in 0..rows.length() {
        let Some(node) = rows.item(i) else { continue };
        let el: web_sys::Element = node.unchecked_into();
        let pos = if by_row {
            diff_row_of(&el)
        } else {
            dline_of(&el)
        };
        if pos.is_some_and(|p| p >= lo && p <= hi) {
            let classes = el.class_list();
            let _ = classes.add_1("line-composing");
            if !by_row {
                let _ = classes.add_1("line-in-range");
            }
        }
    }
}

fn handle_key(state: AppState, ev: web_sys::KeyboardEvent) {
    // Zoom chords live above every other filter: they must work with the
    // composer open, and they bind both Ctrl and Cmd (the same wasm runs
    // on Linux and macOS). Ctrl+Shift+= arrives as key "+" on the Equal
    // code — that's the standard zoom-in, so Shift stays allowed. Not a
    // zoom chord: fall through (Ctrl/Cmd+Enter must still reach the composer).
    if (ev.ctrl_key() || ev.meta_key()) && !ev.alt_key() {
        let step = match ev.code().as_str() {
            "Equal" | "NumpadAdd" => Some(1),
            "Minus" | "NumpadSubtract" => Some(-1),
            "Digit0" | "Numpad0" => Some(0),
            _ => match ev.key().as_str() {
                "=" | "+" => Some(1),
                "-" => Some(-1),
                "0" => Some(0),
                _ => None,
            },
        };
        if let Some(step) = step {
            let cur = state.zoom.get_untracked();
            let next = match step {
                1 => zoom_in(cur),
                -1 => zoom_out(cur),
                _ => DEFAULT_ZOOM,
            };
            state.zoom.set(next);
            state.say(format!("Zoom {}%", (next * 100.0).round()));
            ev.prevent_default();
            return;
        }
    }
    // While the composer is open it owns the keyboard: Esc cancels (the
    // draft is kept), Ctrl/Cmd+Enter submits, Ctrl/Cmd+Shift+Backspace
    // deletes the edited comment, everything else is text input.
    if state.composer.with_untracked(|c| c.is_some()) {
        match ev.key().as_str() {
            "Escape" => composer::close_composer(state),
            "Enter" if ev.ctrl_key() || ev.meta_key() => composer::submit_comment(state),
            "Backspace" if (ev.ctrl_key() || ev.meta_key()) && ev.shift_key() => {
                composer::delete_open_comment(state);
            }
            _ => {}
        }
        return;
    }

    // While the help dialog is open it is modal: only its own dismissers
    // act; navigation keys must not move review focus behind the overlay.
    if state.help_open.get_untracked() {
        match ev.key().as_str() {
            "?" | "Escape" => state.help_open.set(false),
            _ => {}
        }
        return;
    }
    if ev.ctrl_key() || ev.meta_key() || ev.alt_key() {
        return;
    }
    let key = ev.key();
    match key.as_str() {
        "j" | "k" | "ArrowDown" | "ArrowUp" => {
            let d = if key == "j" || key == "ArrowDown" {
                1
            } else {
                -1
            };
            if let Some(t) = step_visible(
                &state.visible.get_untracked(),
                state.focused.get_untracked(),
                d,
            ) {
                state.key_focus(t);
            }
            ev.prevent_default();
        }
        "J" | "K" => {
            let d = if key == "J" { 1 } else { -1 };
            let commented: Vec<usize> = state
                .visible
                .get_untracked()
                .into_iter()
                .filter(|&oi| state.map.with_untracked(|m| m.outline[oi].commented))
                .collect();
            if let Some(t) = step_commented(&commented, state.focused.get_untracked(), d) {
                state.key_focus(t);
            }
            ev.prevent_default();
        }
        "<" | ">" => {
            let cur = state.tier.get_untracked();
            let next = state.tiers.with_untracked(|ts| {
                if key == ">" {
                    ts.iter().copied().find(|&t| t > cur)
                } else {
                    ts.iter().rev().copied().find(|&t| t < cur)
                }
            });
            if let Some(t) = next {
                state.tier.set(t);
                state.say(format!("Showing tiers P0 through P{t}"));
            }
            ev.prevent_default();
        }
        "v" => {
            let cur = state.focused.get_untracked();
            let exists = state.map.with_untracked(|m| cur < m.hunks.len());
            if !exists {
                return;
            }
            if toggle_viewed(state, cur)
                && let Some(t) = advance_after_viewed(&state.visible.get_untracked(), cur)
            {
                state.focused.set(t);
                defer_scroll_focus_into_view(t);
            }
            ev.prevent_default();
        }
        "e" => {
            open_in_editor(state, state.focused.get_untracked());
            ev.prevent_default();
        }
        "t" => {
            let cur = state.focused.get_untracked();
            let map_idx = state
                .map
                .with_untracked(|m| m.hunks.get(cur).map(|h| h.map_idx));
            if let Some(mi) = map_idx {
                let mut folded = false;
                state.collapsed.update(|c| {
                    if !c.remove(&mi) {
                        c.insert(mi);
                        folded = true;
                    }
                });
                state.clear_failed(cur);
                if folded {
                    defer_scroll_focus_into_view(cur);
                }
            }
            ev.prevent_default();
        }
        "f" | "F" => {
            state.outline_open.update(|o| *o = !*o);
            ev.prevent_default();
        }
        "?" => {
            state.help_open.update(|h| *h = !*h);
        }
        "Escape" => {
            state.help_open.set(false);
        }
        _ => {}
    }
}

/// The rAF tick after one (or several coalesced) wheel events: re-derive
/// focus from hunk-head geometry against the pane midline when the wheel
/// moved the pane, or advance one step when the pane is clamped at an edge.
/// `pane_top` holds the offset the last tick settled on; an advance's
/// programmatic scroll re-stores it so a not-yet-settled viewport can't
/// re-trigger derivation and snap focus back.
fn wheel_tick(state: AppState, dy: f64, pane_top: &Cell<i32>) {
    let Some(pane) = web_document().and_then(|d| d.query_selector(".detail").ok().flatten()) else {
        return;
    };
    let st = pane.scroll_top();
    let moved = st != pane_top.get();
    let rect = pane.get_bounding_client_rect();
    let anchor = rect.top() + rect.height() / 2.0;
    let Ok(nodes) = pane.query_selector_all(".hunk") else {
        return;
    };
    let mut heads = Vec::with_capacity(nodes.length() as usize);
    let mut ois = Vec::with_capacity(nodes.length() as usize);
    for i in 0..nodes.length() {
        let Some(node) = nodes.item(i) else { continue };
        let el: web_sys::Element = node.unchecked_into();
        let Some(oi) = el
            .id()
            .strip_prefix("hunk-")
            .and_then(|s| s.parse::<usize>().ok())
        else {
            continue;
        };
        heads.push(el.get_bounding_client_rect().top());
        ois.push(oi);
    }
    let Some(derived) = derive_focus(&heads, anchor).map(|pos| ois[pos]) else {
        return;
    };
    let cur = state.focused.get_untracked();
    let at_bottom =
        f64::from(st) + f64::from(pane.client_height()) >= f64::from(pane.scroll_height()) - 1.0;
    let at_top = st <= 0;
    // Vertical overflow at all? Without it both edges read true and a
    // spurious `moved` first tick (pane_top starts unseeded) must not
    // trigger decide_wheel_focus's edge settle.
    let scrollable = pane.scroll_height() > pane.client_height();
    let visible = state.visible.get_untracked();
    let (Some(&first), Some(&last)) = (visible.first(), visible.last()) else {
        return;
    };
    let next = decide_wheel_focus(
        WheelTick {
            geometry_moved: moved,
            derived,
            cur,
            dy,
            at_top,
            at_bottom,
            scrollable,
            first,
            last,
        },
        |d| step_visible(&visible, cur, d),
    );
    pane_top.set(st);
    if let Some((oi, scroll)) = next {
        state.wheel_focus(oi);
        if scroll {
            scroll_focus_into_view(oi);
            pane_top.set(pane.scroll_top());
        }
    }
}

/// True when the platform reports prefers-reduced-motion. Gates explicit
/// smooth scrolling (comment-row centering); plain scroll-focus jumps are
/// instant already (`scroll-behavior` is never set in the sheet).
pub fn reduced_motion() -> bool {
    web_sys::window()
        .and_then(|w| {
            w.match_media("(prefers-reduced-motion: reduce)")
                .ok()
                .flatten()
        })
        .is_some_and(|m| m.matches())
}

/// True when hunk `oi`'s head already rests in the pane's upper half —
/// where a first-hunk initial focus sits at the natural scrollTop=0.
/// Skipping the initial scroll there keeps scroll_into_view's
/// scroll-margin alignment from dragging the pane a few px down.
fn head_in_upper_pane(oi: usize) -> bool {
    let Some(document) = web_document() else {
        return false;
    };
    let Some(pane) = document.query_selector(".detail").ok().flatten() else {
        return false;
    };
    let Ok(Some(head)) = document.query_selector(&format!("#hunk-{oi} .hunk-head")) else {
        return false;
    };
    let pane_rect = pane.get_bounding_client_rect();
    let top = head.get_bounding_client_rect().top() - pane_rect.top();
    (0.0..=pane_rect.height() / 2.0).contains(&top)
}

/// Scroll effect: reads `focused`'s target, never writes it. Hunk head goes
/// to the pane top (`scroll-margin-top` offsets it), outline row to the
/// nearest edge so the sidebar doesn't jump.
pub fn scroll_focus_into_view(oi: usize) {
    let Some(document) = web_document() else {
        return;
    };
    if let Ok(Some(head)) = document.query_selector(&format!("#hunk-{oi} .hunk-head")) {
        let opts = web_sys::ScrollIntoViewOptions::new();
        opts.set_block(web_sys::ScrollLogicalPosition::Start);
        head.scroll_into_view_with_scroll_into_view_options(&opts);
    }
    if let Ok(Some(item)) = document.query_selector(&format!(".outline-item[data-idx='{oi}']")) {
        let opts = web_sys::ScrollIntoViewOptions::new();
        opts.set_block(web_sys::ScrollLogicalPosition::Nearest);
        item.scroll_into_view_with_scroll_into_view_options(&opts);
    }
}

/// Post-flush [`scroll_focus_into_view`]. Collapse-triggering actions
/// (mark-viewed fold, `t`, the chevron) write `collapsed` synchronously,
/// but Leptos applies the `.collapsed` class — and with it the reflow
/// dragging every lower hunk up — on the next render flush, so a scroll
/// computed in the same task would target stale pre-fold geometry. One
/// rAF runs after the flush, still before paint.
pub fn defer_scroll_focus_into_view(oi: usize) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let cb = Closure::once_into_js(move || scroll_focus_into_view(oi));
    let _ = window.request_animation_frame(cb.unchecked_ref());
}

/// Optimistic viewed toggle on hunk `oi`: flips the local set instantly,
/// folds the hunk when marking, then reconciles with the returned
/// `ViewedState`. Returns whether the hunk was marked (true) or unmarked.
pub fn toggle_viewed(state: AppState, oi: usize) -> bool {
    let (file, row, map_idx) = state.map.with_untracked(|m| {
        let h = &m.hunks[oi];
        (h.file.clone(), h.row_start, h.map_idx)
    });
    let identity = hunk_identity(&file, row);
    let marking = !state.viewed.with_untracked(|v| v.contains(&identity));
    state.viewed.update(|v| {
        if marking {
            v.insert(identity);
        } else {
            v.remove(&identity);
        }
    });
    state.say(if marking {
        format!("Marked viewed {file} row {row}")
    } else {
        format!("Marked unviewed {file} row {row}")
    });
    state.collapsed.update(|c| {
        if marking {
            c.insert(map_idx);
        } else {
            c.remove(&map_idx);
        }
    });
    leptos::task::spawn_local(async move {
        #[derive(serde::Serialize)]
        struct SetViewedArgs {
            file: String,
            row: u32,
        }
        match invoke::invoke_args::<ViewedState, _>("set_viewed", &SetViewedArgs { file, row })
            .await
        {
            Ok(vs) => state.viewed.set(vs.viewed.into_iter().collect()),
            Err(err) => leptos::logging::warn!("set_viewed failed: {err}"),
        }
    });
    marking
}

/// `$EDITOR` for the hunk's file at its first row (the backend maps the row
/// to a file line) — a silent no-op unless the backend reported a usable
/// editor and the file openable, matching the button not being rendered.
/// A failed launch is announced.
pub fn open_in_editor(state: AppState, oi: usize) {
    let Some((file, row)) = state.map.with_untracked(|m| {
        let h = m.hunks.get(oi)?;
        (m.editor.is_some() && h.openable).then(|| (h.file.clone(), h.row_start))
    }) else {
        return;
    };
    leptos::task::spawn_local(async move {
        #[derive(serde::Serialize)]
        struct OpenArgs {
            path: String,
            row: Option<u32>,
        }
        if let Err(err) = invoke::invoke_args::<serde_json::Value, _>(
            "open_in_editor",
            &OpenArgs {
                path: file,
                row: Some(row),
            },
        )
        .await
        {
            state.say(format!("Opening the editor failed: {err}"));
        }
    });
}
