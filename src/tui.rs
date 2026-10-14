//! Interactive terminal viewer for Adhdiff — the same review surface as
//! the Tauri desktop UI, rendered in the terminal.
//!
//! Layout:
//!   TOP:     title + summary.
//!   LEFT:    tier filter chips + outline of hunks in reading order
//!            (supports-tree indent, viewed ✓, comment ◆n).
//!   RIGHT:   detail column — one card per visible hunk: tier badge,
//!            label, file:loc, rationale, syntax-highlighted diff body
//!            with gutter line numbers, comment rows under the card.
//!   BOTTOM:  status bar, or the comment composer while open.
//!
//! All review state lives in `crate::state` and persists immediately to
//! the shared sidecar (`$XDG_STATE_HOME/adhdiff/<worktree>/<sha>.json` or
//! --out), so the TUI and the GUI see each other's
//! comments and viewed marks.
//!
//! Keys (? shows the same list in-app):
//!   j / k / ↓ / ↑ / n / p  prev/next hunk (skips filtered tiers)
//!   J / K                  page-scroll the detail pane
//!   v                      toggle viewed (persisted; collapses the card)
//!   e                      open focused file in $VISUAL/$EDITOR
//!   c                      comment on focused hunk (Ctrl+Enter/Ctrl+S save,
//!                          Esc cancels; Enter inserts a newline)
//!   ← / →                  choose comment row on the focused card
//!   x / d                  delete the chosen comment (persisted)
//!   1..8                   filter+pounce: show up to tier P(n-1), jump there
//!   < / >                  show less / more tiers (P0 → loosest)
//!   F                      cycle the tier filter
//!   t                      collapse/expand the focused card
//!   f                      toggle the outline pane
//!   ?                      help overlay
//!   q                      quit

use std::collections::HashSet;
use std::io;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::diff::{find_raw_for, parse_unified, row_to_new_line};
use crate::highlight::{Class, Highlighter, Span as HlSpan};
use crate::state::{self, Inner, comment_key, comments_at, row_pins_for};
use adhdiff_shared::{AdhDiffMap, Confidence, LabeledHunk, SavedComment};

/// GitHub-dark palette approximations, matching serve's CSS / gui's `col`.
mod col {
    use ratatui::style::Color;
    pub const FG: Color = Color::Rgb(0xe6, 0xed, 0xf3);
    pub const FG_DIM: Color = Color::Rgb(0x7d, 0x85, 0x90);
    pub const BORDER: Color = Color::Rgb(0x30, 0x36, 0x3d);
    pub const PANEL2: Color = Color::Rgb(0x21, 0x26, 0x2d);
    pub const ACCENT: Color = Color::Rgb(0x58, 0xa6, 0xff);
    /// #238636 @ ~10% over #0d1117 (web's add row tint, alpha flattened).
    pub const ADD_BG: Color = Color::Rgb(0x0f, 0x1d, 0x1a);
    /// #da3633 @ ~10% over #0d1117 (web's del row tint, alpha flattened).
    pub const DEL_BG: Color = Color::Rgb(0x22, 0x15, 0x1a);
}

/// Tier badge color, matching serve's tier-pN classes and gui's tier_color.
fn tier_rgb(tier: u8) -> Color {
    match tier {
        0 => Color::Rgb(0xff, 0x7b, 0x72),
        1 => Color::Rgb(0xf2, 0xcc, 0x60),
        2 => Color::Rgb(0x79, 0xc0, 0xff),
        3 => Color::Rgb(0xd2, 0xa8, 0xff),
        4 => Color::Rgb(0x56, 0xd4, 0xdd),
        _ => Color::Rgb(0x8b, 0x94, 0x9e),
    }
}

/// What `?` shows. Mirrors gui's HELP_TEXT with the TUI's deltas.
const HELP_TEXT: &str = r#"adhdiff tui — keymap

NAVIGATION
  j / ↓          next hunk
  k / ↑          previous hunk
  n / p          next / previous hunk
  J / K          page-scroll the detail pane

VIEW
  ?              this help
  Esc            close overlay/composer
  t              collapse/expand focused hunk
  f              toggle the left outline panel
  < / >          show less / more (P0 → loosest)
  F              cycle the tier filter
  1..8           filter to ≤ P(n-1) and jump to that tier

ACTIONS
  v              toggle viewed (focused hunk)
  e              open focused file in $EDITOR
  c              comment on focused hunk
  ← / →          choose a comment on the focused card
  x / d          delete the chosen comment
  Ctrl+Enter     save comment (Ctrl+S also saves — some
                 terminals cannot tell Ctrl+Enter from Enter)
  Enter          newline inside the comment composer
  q / Ctrl+C     quit
"#;

/// One rendered diff body row, prepared once at startup (bodies are
/// session-immutable; highlights would be identical every frame).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    Add,
    Del,
    Ctx,
    Warn,
}

#[derive(Debug, Clone)]
struct BodyRow {
    kind: RowKind,
    /// Gutter text: new-side line number (old-side for deletes).
    gutter: String,
    /// Diff row of this body line; `None` for the synthetic "more lines"
    /// footer.
    row: Option<u32>,
    /// Highlighted runs for the code text.
    spans: Vec<(String, Color)>,
}

#[derive(Debug)]
struct Composer {
    oi: usize,
    file: String,
    row: u32,
    end_row: Option<u32>,
    buffer: String,
}

struct App {
    inner: Inner,
    /// One card per RAW hunk, in reading order (segment entries hang
    /// under their anchor; the whole body renders once per card).
    groups: Vec<crate::outline::RawGroup>,
    /// Group-level supports-tree parents, indexed like `groups`.
    parents: Vec<Option<usize>>,
    /// Highlighted body rows per group, indexed by group position.
    bodies: Vec<Vec<BodyRow>>,
    /// Selected outline index (the web's `current`, gui's `selected`).
    selected: usize,
    /// Tier filter: hunks at tiers ≤ max_tier are visible.
    max_tier: u8,
    outline_visible: bool,
    help_open: bool,
    /// Collapsed outline indices; viewed hunks start collapsed (gui parity).
    collapsed: HashSet<usize>,
    /// Which comment row of the focused card x/d acts on.
    comment_cursor: usize,
    composer: Option<Composer>,
    /// Detail pane scroll offset (rows), clamped against the assembled
    /// content. `follow` asks the next draw to reveal the selected card.
    detail_scroll: u16,
    follow: bool,
    /// Filled by draw: last viewport height and content height + card spans
    /// (oi, first row, one past last row).
    viewport: u16,
    content: u16,
    card_spans: Vec<(usize, u16, u16)>,
    /// Transient footer notice (e.g. "comment saved").
    status: Option<(Instant, String)>,
    /// e pressed: (file, line) to open once the event loop can drop the
    /// terminal for a terminal editor (suspend/resume dance) or spawn a
    /// windowed editor detached.
    pending_editor: Option<(String, u32)>,
    quit: bool,
}

impl App {
    fn new(inner: Inner) -> Self {
        let groups = crate::outline::raw_groups(&inner.map, &inner.files);
        let parents: Vec<Option<usize>> = groups.iter().map(|g| g.parent).collect();
        let bodies = groups.iter().map(|gr| hunk_body(&inner, gr)).collect();
        let max_tier = groups
            .iter()
            .map(|g| g.priority)
            .max()
            .map(|p| p.as_u8())
            .unwrap_or(0);
        let mut collapsed = HashSet::new();
        for (oi, gr) in groups.iter().enumerate() {
            if inner.viewed.contains(&gr.identity()) {
                collapsed.insert(oi);
            }
        }
        Self {
            inner,
            groups,
            parents,
            bodies,
            selected: 0,
            max_tier,
            outline_visible: true,
            help_open: false,
            collapsed,
            comment_cursor: 0,
            composer: None,
            detail_scroll: 0,
            follow: true,
            viewport: 0,
            content: 0,
            card_spans: Vec::new(),
            status: None,
            pending_editor: None,
            quit: false,
        }
    }

    fn len(&self) -> usize {
        self.groups.len()
    }

    /// The group's anchor entry.
    fn anchor(&self, oi: usize) -> &LabeledHunk {
        &self.inner.map.hunks[self.groups[oi].anchor]
    }

    /// The group's display entry: max-tier segment (tie: earliest
    /// start) supplies label/rationale.
    fn label_entry(&self, oi: usize) -> &LabeledHunk {
        &self.inner.map.hunks[self.groups[oi].label_idx]
    }

    fn file_key(&self, oi: usize) -> String {
        self.groups[oi].file.clone()
    }

    fn visible(&self, oi: usize) -> bool {
        self.groups[oi].priority.as_u8() <= self.max_tier
    }

    /// Comments anchored inside the focused card's raw-hunk span, in
    /// stable (line, ts) order — HashMap iteration order must not leak
    /// into a selection index.
    fn card_comments(&self, oi: usize) -> Vec<SavedComment> {
        let gr = &self.groups[oi];
        let mut cs = comments_at(&self.inner, &gr.file, gr.span.0, gr.span.1);
        cs.sort_by_key(|c| (c.row, c.ts_unix));
        cs
    }

    /// Clamp the comment cursor against `n` comments on the card.
    fn clamp_comment_cursor(&mut self) {
        let n = self.card_comments(self.selected).len();
        if n == 0 {
            self.comment_cursor = 0;
        } else if self.comment_cursor >= n {
            self.comment_cursor = n - 1;
        }
    }

    fn select(&mut self, oi: usize) {
        self.selected = oi;
        self.clamp_comment_cursor();
        self.follow = true;
    }

    /// j/k: step to the next tier-visible hunk; stops at the edges.
    fn move_selection(&mut self, d: i64) {
        let n = self.len() as i64;
        if n == 0 {
            return;
        }
        let mut i = self.selected as i64;
        loop {
            i += d;
            if i < 0 || i >= n {
                return;
            }
            if self.visible(i as usize) {
                self.select(i as usize);
                return;
            }
        }
    }

    /// J/K: page-scroll the detail pane without moving the selection.
    /// Uses the viewport height measured by the previous draw.
    fn page_scroll(&mut self, d: i64) {
        let page = self.viewport.saturating_sub(2).max(1) as i64;
        let max_scroll = self.content.saturating_sub(self.viewport) as i64;
        self.detail_scroll = (self.detail_scroll as i64 + d * page).clamp(0, max_scroll) as u16;
        self.follow = false;
    }

    /// </>: walk the tier filter across the tiers present among the
    /// rendered cards (gui's step_tier_filter — clamped).
    fn step_tier_filter(&mut self, d: i64) {
        let tiers = present_tiers(self);
        if let Some(next) = step_tier(&tiers, self.max_tier, d) {
            self.set_tier_filter(next);
        }
    }

    /// F: cycle the filter to the next present tier, wrapping to the
    /// strictest after the loosest.
    fn cycle_tier_filter(&mut self) {
        let tiers = present_tiers(self);
        if let Some(next) = cycle_tier(&tiers, self.max_tier) {
            self.set_tier_filter(next);
        }
    }

    /// 1..8: show up to tier P(n-1) and jump to its first hunk, when that
    /// tier is present among the rendered cards.
    fn filter_to_tier(&mut self, tier: u8) {
        if !present_tiers(self).contains(&tier) {
            return;
        }
        self.set_tier_filter(tier);
        if let Some(oi) = (0..self.len()).find(|&oi| self.groups[oi].priority.as_u8() == tier) {
            self.select(oi);
        }
    }

    fn set_tier_filter(&mut self, tier: u8) {
        self.max_tier = tier;
        self.normalize_selection();
    }

    /// After a filter change the selection can point at a hidden hunk;
    /// move it to the nearest visible one.
    fn normalize_selection(&mut self) {
        if self.len() == 0 || self.visible(self.selected) {
            return;
        }
        let fwd = (self.selected + 1..self.len()).find(|&i| self.visible(i));
        let back = (0..self.selected).rev().find(|&i| self.visible(i));
        if let Some(oi) = fwd.or(back) {
            self.select(oi);
        }
    }

    fn toggle_collapsed(&mut self, oi: usize) {
        if !self.collapsed.remove(&oi) {
            self.collapsed.insert(oi);
        }
    }

    /// v. Mirrors the gui: marking a hunk viewed collapses it and advances
    /// focus to the next visible hunk; unmarking expands it. Persists
    /// immediately to the shared sidecar.
    fn toggle_viewed(&mut self, oi: usize) {
        if oi >= self.len() {
            return;
        }
        let id = self.groups[oi].identity();
        let marking = !self.inner.viewed.contains(&id);
        if marking {
            self.inner.viewed.insert(id);
            self.collapsed.insert(oi);
        } else {
            self.inner.viewed.remove(&id);
            self.collapsed.remove(&oi);
        }
        let ok = self.persist();
        if ok {
            self.flash(if marking {
                "marked viewed"
            } else {
                "unmarked viewed"
            });
        }
        if marking {
            let n = self.len();
            let next = (oi + 1..n)
                .find(|&i| self.visible(i))
                .or_else(|| (0..n).find(|&i| self.visible(i)));
            if let Some(next) = next {
                self.select(next);
            }
        }
    }

    /// e: queue an open-in-editor jump at the hunk's first row. The event
    /// loop performs it — detached for windowed editors, with the
    /// suspend/resume dance for terminal editors. Silent when no worktree
    /// is bound, same as the web/gui.
    fn request_editor(&mut self, oi: usize) {
        if oi >= self.len() || self.inner.workdir_hint.is_none() {
            return;
        }
        let gr = &self.groups[oi];
        let line = find_raw_for(&self.inner.files, gr.span.0)
            .map_or(1, |(_, raw)| row_to_new_line(raw, gr.span.0));
        self.pending_editor = Some((gr.file.clone(), line));
    }

    /// The comment whose range covers `row` in `file` — serve/gui's
    /// one-comment-per-row overlap adoption.
    fn existing_comment_at(&self, file: &str, row: u32) -> Option<&SavedComment> {
        self.inner
            .comments
            .values()
            .flatten()
            .find(|c| c.file == file && c.row <= row && c.end_row.unwrap_or(c.row) >= row)
    }

    /// c: open the composer anchored at the hunk's first row (the TUI has
    /// no per-row cursor). A comment already on that row gets adopted —
    /// span and body prefilled, saving overwrites it in place.
    fn open_composer(&mut self, oi: usize) {
        let file = self.file_key(oi);
        let row = self.groups[oi].span.0;
        let (row, end_row, body) = match self.existing_comment_at(&file, row) {
            Some(c) => (c.row, c.end_row, c.body.clone()),
            None => (row, None, String::new()),
        };
        self.composer = Some(Composer {
            oi,
            file,
            row,
            end_row,
            buffer: body,
        });
        self.help_open = false;
    }

    /// Ctrl+Enter: persist via the shared state module — the same key
    /// shape, replace-overlapping rule and sidecar file serve/gui use.
    fn submit_composer(&mut self) {
        let Some(comp) = self.composer.take() else {
            return;
        };
        let body = comp.buffer.trim().to_string();
        if body.is_empty() {
            return;
        }
        let end_row = comp.end_row.filter(|&e| e > comp.row);
        let key = comment_key(&comp.file, comp.row, end_row);
        let label_at_time = self
            .inner
            .map
            .hunks
            .iter()
            .find(|h| h.row_start <= comp.row && comp.row <= h.row_end)
            .map(|h| h.label.clone())
            .unwrap_or_default();
        let c = SavedComment {
            id: state::short_id(),
            file: comp.file.clone(),
            row: comp.row,
            end_row,
            label_at_time,
            author: "human".into(),
            body,
            ts_unix: state::now_unix(),
        };
        // At most one comment per row: drop every comment in this file
        // whose range intersects the new one, then store under its own key.
        let new_end = end_row.unwrap_or(comp.row);
        self.inner.comments.retain(|_, cs| {
            !cs.iter().any(|old| {
                old.file == c.file && old.row <= new_end && old.end_row.unwrap_or(old.row) >= c.row
            })
        });
        self.inner.comments.entry(key).or_default().push(c);
        if self.persist() {
            self.flash("comment saved");
        }
        self.clamp_comment_cursor();
    }

    /// x/d on the chosen comment row of the focused card. Serve parity:
    /// every comment sharing the row's (file, row) goes.
    fn delete_selected_comment(&mut self) {
        if self.len() == 0 {
            return;
        }
        let Some(c) = self
            .card_comments(self.selected)
            .get(self.comment_cursor)
            .cloned()
        else {
            return;
        };
        // Same removal rule as serve's DELETE handler and gui's ×.
        let mut removed = 0;
        self.inner.comments.retain(|_, cs| {
            let before = cs.len();
            cs.retain(|x| !(x.file == c.file && x.row == c.row));
            removed += before - cs.len();
            !cs.is_empty()
        });
        if removed == 0 {
            return;
        }
        if self.persist() {
            self.flash(&if removed == 1 {
                "comment deleted".to_string()
            } else {
                format!("deleted {removed} comments")
            });
        }
        self.clamp_comment_cursor();
    }

    /// Returns true when the sidecar accepted the write; on failure the
    /// error flash must not be overwritten by a success flash.
    fn persist(&mut self) -> bool {
        match state::persist_sidecar(&self.inner) {
            Ok(()) => true,
            Err(e) => {
                self.flash(&format!("persist failed: {e:#}"));
                false
            }
        }
    }

    fn flash(&mut self, msg: &str) {
        self.status = Some((Instant::now(), msg.to_string()));
    }
}

/// Indent depth under supports ancestors (0 = root), cycle-bounded like
/// serve's outline_depth.
fn outline_depth(parent: &[Option<usize>], mi: usize) -> usize {
    let mut d = 0;
    let mut cur = mi;
    while let Some(p) = parent[cur] {
        if d >= parent.len() {
            break;
        }
        d += 1;
        cur = p;
    }
    d
}

/// Tiers present among the rendered cards (one per raw hunk), ascending.
fn present_tiers(app: &App) -> Vec<u8> {
    let mut tiers: Vec<u8> = app.groups.iter().map(|g| g.priority.as_u8()).collect();
    tiers.sort_unstable();
    tiers.dedup();
    tiers
}

/// gui's step_tier_filter: clamped walk across present tiers.
fn step_tier(tiers: &[u8], cur: u8, d: i64) -> Option<u8> {
    if tiers.is_empty() {
        return None;
    }
    let idx = tiers
        .iter()
        .position(|&t| t >= cur)
        .unwrap_or(tiers.len() - 1) as i64;
    Some(tiers[(idx + d).clamp(0, tiers.len() as i64 - 1) as usize])
}

/// F: wrapping walk across present tiers.
fn cycle_tier(tiers: &[u8], cur: u8) -> Option<u8> {
    if tiers.is_empty() {
        return None;
    }
    let idx = tiers
        .iter()
        .position(|&t| t >= cur)
        .unwrap_or(tiers.len() - 1);
    Some(tiers[(idx + 1) % tiers.len()])
}

fn hunk_body(g: &Inner, gr: &crate::outline::RawGroup) -> Vec<BodyRow> {
    let Some((_, raw)) = find_raw_for(&g.files, gr.span.0) else {
        return Vec::new();
    };

    let hl = Highlighter::for_hunk(&gr.file, &raw.body);
    let mut rows: Vec<BodyRow> = Vec::new();
    let mut new_line_no = raw.new_start;
    let mut old_line_no = if raw.old_start > 0 { raw.old_start } else { 0 };
    for (i, l) in raw.body.lines().enumerate() {
        // Cap absurdly long bodies; the card footer reports the omission.
        if rows.len() >= 1000 {
            break;
        }
        let row = raw
            .row_start
            .saturating_add(u32::try_from(i).unwrap_or(u32::MAX));
        let (kind, gutter, text) = if let Some(rest) = l.strip_prefix('+') {
            let ln = new_line_no;
            new_line_no = new_line_no.saturating_add(1);
            (RowKind::Add, ln.to_string(), rest)
        } else if let Some(rest) = l.strip_prefix('-') {
            let ln = old_line_no;
            old_line_no = old_line_no.saturating_add(1);
            (RowKind::Del, ln.to_string(), rest)
        } else if l.starts_with('\\') {
            (RowKind::Warn, String::new(), l)
        } else {
            let rest = l.strip_prefix(' ').unwrap_or(l);
            let ln = new_line_no;
            new_line_no = new_line_no.saturating_add(1);
            old_line_no = old_line_no.saturating_add(1);
            (RowKind::Ctx, ln.to_string(), rest)
        };
        let spans = highlight_spans(hl.spans(i), text);
        rows.push(BodyRow {
            kind,
            gutter,
            row: Some(row),
            spans,
        });
    }
    let omitted = raw.body.lines().count().saturating_sub(rows.len());
    if omitted > 0 {
        rows.push(BodyRow {
            kind: RowKind::Warn,
            gutter: String::new(),
            row: None,
            spans: vec![(format!("… {omitted} more body lines"), col::FG_DIM)],
        });
    }
    rows
}

/// Highlight one line → (text, Color) runs; once per line ever (bodies
/// are cached for the session).
fn highlight_spans(spans: &[HlSpan], text: &str) -> Vec<(String, Color)> {
    let mut runs = Vec::new();
    let mut at = 0;
    for &(s, e, class) in spans {
        let (Some(gap), Some(body)) = (text.get(at..s), text.get(s..e)) else {
            continue;
        };
        if !gap.is_empty() {
            runs.push((gap.to_string(), col::FG));
        }
        runs.push((body.to_string(), class_color(class)));
        at = e;
    }
    let tail = text.get(at..).unwrap_or("");
    if !tail.is_empty() || runs.is_empty() {
        runs.push((tail.to_string(), col::FG));
    }
    runs
}

fn class_color(class: Class) -> Color {
    match class {
        Class::Keyword | Class::Operator => Color::Rgb(0xff, 0x7b, 0x72),
        Class::String => Color::Rgb(0xa5, 0xd6, 0xff),
        Class::Comment => Color::Rgb(0x8b, 0x94, 0x9e),
        Class::Function => Color::Rgb(0xd2, 0xa8, 0xff),
        Class::Type | Class::Variable => Color::Rgb(0xff, 0xa6, 0x57),
        Class::Number | Class::Property => Color::Rgb(0x79, 0xc0, 0xff),
        Class::Constant => Color::Rgb(0x56, 0xd4, 0xdd),
        Class::Punctuation => Color::Rgb(0xc9, 0xd1, 0xd9),
        Class::Attribute => Color::Rgb(0xd2, 0x99, 0x22),
        Class::Tag => Color::Rgb(0x7e, 0xe7, 0x87),
        Class::Label => Color::Rgb(0xf2, 0xcc, 0x60),
    }
}

// ---------- key handling (pure mappings, unit-tested) ----------

/// Root-key actions — the full parity key set, mapped without touching
/// app state so the mapping itself is testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Quit,
    Move(i64),
    PageScroll(i64),
    CommentCursor(i64),
    ToggleViewed,
    OpenEditor,
    OpenComposer,
    ToggleCollapsed,
    ToggleOutline,
    CycleFilter,
    StepFilter(i64),
    FilterTier(u8),
    ToggleHelp,
    CloseOverlay,
    DeleteComment,
}

fn root_action(code: KeyCode, mods: KeyModifiers) -> Option<Action> {
    // Ctrl+C quits like it does outside raw mode; other Ctrl/Alt combos
    // are not letter bindings.
    if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
        return Some(Action::Quit);
    }
    if mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return None;
    }
    Some(match code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('j') | KeyCode::Down | KeyCode::Char('n') => Action::Move(1),
        KeyCode::Char('k') | KeyCode::Up | KeyCode::Char('p') => Action::Move(-1),
        KeyCode::Char('J') => Action::PageScroll(1),
        KeyCode::Char('K') => Action::PageScroll(-1),
        KeyCode::Left => Action::CommentCursor(-1),
        KeyCode::Right => Action::CommentCursor(1),
        KeyCode::PageDown => Action::PageScroll(1),
        KeyCode::PageUp => Action::PageScroll(-1),
        KeyCode::Char('<') => Action::StepFilter(-1),
        KeyCode::Char('>') => Action::StepFilter(1),
        KeyCode::Char('F') => Action::CycleFilter,
        KeyCode::Char(ch @ '1'..='8') => Action::FilterTier(ch as u8 - b'1'),
        KeyCode::Char('v') => Action::ToggleViewed,
        KeyCode::Char('e') => Action::OpenEditor,
        KeyCode::Char('c') => Action::OpenComposer,
        KeyCode::Char('t') => Action::ToggleCollapsed,
        KeyCode::Char('f') => Action::ToggleOutline,
        KeyCode::Char('?') => Action::ToggleHelp,
        KeyCode::Char('x') | KeyCode::Char('d') => Action::DeleteComment,
        KeyCode::Esc => Action::CloseOverlay,
        _ => return None,
    })
}

/// Composer-key outcomes — editing keys must never drop the draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerKey {
    Submit,
    Cancel,
    Newline,
    Push(char),
    Backspace,
    Ignore,
}

fn composer_key(code: KeyCode, mods: KeyModifiers) -> ComposerKey {
    match code {
        KeyCode::Esc => ComposerKey::Cancel,
        // Ctrl+Enter requires kitty-protocol enhancement (pushed at
        // startup); Ctrl+S is the fallback for terminals that cannot
        // report it. Flow control (XON/XOFF) is off in raw mode.
        KeyCode::Enter if mods.contains(KeyModifiers::CONTROL) => ComposerKey::Submit,
        KeyCode::Char('s') if mods.contains(KeyModifiers::CONTROL) => ComposerKey::Submit,
        KeyCode::Enter => ComposerKey::Newline,
        KeyCode::Backspace => ComposerKey::Backspace,
        KeyCode::Char(ch) if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            ComposerKey::Push(ch)
        }
        _ => ComposerKey::Ignore,
    }
}

/// Scroll so [start, end) is inside [scroll, scroll+viewport), moving the
/// minimum distance and never past either end. Cards taller than the
/// viewport anchor at their start — reading a card from its header beats
/// landing mid-body.
fn ensure_visible(scroll: u16, viewport: u16, start: u16, end: u16) -> u16 {
    if viewport == 0 {
        return start;
    }
    if start < scroll || end.saturating_sub(start) >= viewport {
        start
    } else if end.saturating_sub(1) >= scroll + viewport {
        end.saturating_sub(viewport)
    } else {
        scroll
    }
}

// ---------- terminal plumbing ----------

/// Escape-sequence restore shared by the panic guard's Drop and the
/// normal exit path: pop the kitty enhancement level, leave the alternate
/// screen, show the cursor. Kept byte-exact so the unit test pins it.
fn write_restore<W: io::Write>(w: &mut W) {
    let _ = execute!(
        w,
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

fn restore_terminal() {
    write_restore(&mut io::stdout());
    disable_raw_mode().ok();
}

/// Dropped on the way out of `run_tui` — including during panic unwinds,
/// so a panic can never strand the terminal in raw/alt-screen mode.
struct TermRestore;
impl Drop for TermRestore {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// `out` mirrors gui::run_app's param: the sidecar directory or file.
pub fn run_tui(map: AdhDiffMap, diff: &[u8], out: Option<String>) -> Result<()> {
    // The same splice of parse → sha → reading order → sidecar load that
    // serve's run_serve does.
    let files = parse_unified(diff)?;
    let diff_sha = state::hex_sha256(diff);
    let workdir_hint = state::detect_workdir();
    let (comments, viewed, state_path, load_warn) =
        state::load(&diff_sha, out.as_deref(), workdir_hint.as_deref())?;
    if let Some(w) = &load_warn {
        eprintln!("adhdiff tui: {w}");
    }
    let editor = workdir_hint
        .as_ref()
        .and_then(|_| crate::editor::resolve_editor(crate::editor::Surface::Tui));
    let inner = Inner {
        map,
        files,
        comments,
        viewed,
        state_path,
        diff_sha,
        workdir_hint,
        editor,
        body_cache: Default::default(),
    };
    eprintln!(
        "adhdiff tui ({} hunks) — state: {}",
        inner.map.hunks.len(),
        inner.state_path.display()
    );
    let mut app = App::new(inner);
    if let Some(w) = load_warn {
        app.flash(&w);
    }
    enable_raw_mode().context("enable_raw_mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("alt screen")?;
    let _guard = TermRestore;
    // Ask for kitty-protocol key disambiguation so Ctrl+Enter is distinct
    // from Enter where the terminal supports it (plain nop elsewhere).
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;
    term.clear()?;

    let result = event_loop(&mut term, &mut app);

    restore_terminal();
    std::mem::forget(_guard); // already restored; never double-pop kitty
    result
}

fn event_loop(term: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    loop {
        term.draw(|f| draw(f, app))?;
        if app.quit {
            return Ok(());
        }
        if event::poll(Duration::from_millis(200))?
            && let Event::Key(k) = event::read()?
        {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            handle_key(app, k.code, k.modifiers);
        }
        if let Some((file, line)) = app.pending_editor.take() {
            run_editor(term, app, &file, line);
        }
    }
}

/// Editor stems that manage their own window get the detached spawn.
/// Everything else (vi/vim/nvim/nano/emacs/emacsclient/helix/hx/kak, and
/// any unknown stem) is treated as a terminal editor: the TUI suspends,
/// the editor takes over the foreground, and quitting it resumes the TUI.
/// Unknown → terminal is the safer default: worst case a GUI editor runs
/// foregrounded until closed, which loses nothing.
fn editor_is_windowed(stem: &str) -> bool {
    matches!(
        stem,
        "zed" | "zeditor" | "subl" | "code" | "code-insiders" | "codium" | "fleet" | "atom"
    )
}

/// `e` for the queued hunk: windowed editors spawn detached (as in the
/// GUI); terminal editors run on the real screen between a
/// full suspend and resume of the TUI.
fn run_editor(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    file: &str,
    line: u32,
) {
    let Some(wd) = app.inner.workdir_hint.clone() else {
        return;
    };
    let Some(editor) = app.inner.editor.clone() else {
        app.flash("editor: no usable editor");
        return;
    };
    let stem = std::path::Path::new(editor.split_whitespace().next().unwrap_or_default())
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if editor_is_windowed(&stem) {
        if let Err(why) = crate::editor::open_in_editor(&editor, file, line, &wd) {
            app.flash(&format!("editor: {why}"));
        }
        return;
    }
    restore_terminal();
    let res = crate::editor::open_in_editor_wait(&editor, file, line, &wd);
    // Re-enter, matching the startup install: raw mode, alt screen, kitty
    // disambiguation, then a full clear so the next draw repaints.
    let back = (|| -> Result<()> {
        enable_raw_mode().context("re-enable raw after editor")?;
        execute!(term.backend_mut(), EnterAlternateScreen)?;
        let _ = execute!(
            term.backend_mut(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
        term.clear()?;
        Ok(())
    })();
    if let Err(e) = back {
        app.flash(&format!("terminal resume failed: {e:#}"));
    }
    if let Err(why) = res {
        app.flash(&format!("editor: {why}"));
    }
}

fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    // The composer owns the keyboard while open (web's inComposer guard).
    if let Some(comp) = app.composer.as_mut() {
        match composer_key(code, mods) {
            ComposerKey::Submit => app.submit_composer(),
            ComposerKey::Cancel => {
                app.composer = None;
            }
            ComposerKey::Newline => comp.buffer.push('\n'),
            ComposerKey::Push(c) => comp.buffer.push(c),
            ComposerKey::Backspace => {
                comp.buffer.pop();
            }
            ComposerKey::Ignore => {}
        }
        return;
    }
    let Some(action) = root_action(code, mods) else {
        return;
    };
    match action {
        Action::Quit => app.quit = true,
        Action::Move(d) => {
            app.help_open = false;
            app.move_selection(d);
        }
        Action::PageScroll(d) => app.page_scroll(d),
        Action::CommentCursor(d) => {
            if app.len() == 0 {
                return;
            }
            let n = app.card_comments(app.selected).len();
            if n > 0 {
                let i = app.comment_cursor as i64 + d;
                app.comment_cursor = i.clamp(0, n as i64 - 1) as usize;
            }
        }
        Action::ToggleViewed => app.toggle_viewed(app.selected),
        Action::OpenEditor => app.request_editor(app.selected),
        Action::OpenComposer => {
            if app.len() > 0 {
                let oi = app.selected;
                app.open_composer(oi);
            }
        }
        Action::ToggleCollapsed => {
            if app.len() > 0 {
                app.toggle_collapsed(app.selected);
            }
        }
        Action::ToggleOutline => app.outline_visible = !app.outline_visible,
        Action::CycleFilter => app.cycle_tier_filter(),
        Action::StepFilter(d) => app.step_tier_filter(d),
        Action::FilterTier(t) => app.filter_to_tier(t),
        Action::ToggleHelp => app.help_open = !app.help_open,
        Action::CloseOverlay => {
            if app.help_open {
                app.help_open = false;
            }
        }
        Action::DeleteComment => app.delete_selected_comment(),
    }
}

// ---------- drawing ----------

fn draw(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();
    let footer_len = composer_height(app, area.width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(footer_len),
        ])
        .split(area);

    draw_header(f, app, chunks[0]);

    // Body: outline on the left (toggleable), detail on the right.
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(if app.outline_visible {
            [Constraint::Percentage(32), Constraint::Percentage(68)]
        } else {
            [Constraint::Length(0), Constraint::Percentage(100)]
        })
        .split(chunks[1]);
    draw_outline(f, app, body[0]);
    draw_detail(f, app, body[1]);

    draw_footer(f, app, chunks[2]);

    if app.help_open {
        draw_help(f, area);
    }
}

fn draw_header(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let summary = app.inner.map.summary.lines().next().unwrap_or("");
    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!(" {}", app.inner.map.title),
                Style::default().fg(col::FG).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "   {} viewed · {} comments",
                    app.inner.viewed.len(),
                    app.inner.comments.values().flatten().count()
                ),
                Style::default().fg(col::FG_DIM),
            ),
        ]),
        Line::from(Span::styled(
            format!(" {summary}"),
            Style::default().fg(col::FG_DIM),
        )),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

/// Tier chips row, P0..Pmax with counts, active filter highlighted.
/// Counts are card-level: one card per raw hunk at its derived tier.
fn tier_chips(app: &App) -> Vec<Span<'static>> {
    let mut counts = [0usize; 8];
    for gr in &app.groups {
        counts[gr.priority.as_u8() as usize] += 1;
    }
    let mut spans = vec![Span::raw(" ")];
    for (tier, count) in counts.into_iter().enumerate() {
        if count == 0 {
            continue;
        }
        let tier = tier as u8;
        let style = if tier == app.max_tier {
            Style::default()
                .fg(Color::Black)
                .bg(tier_rgb(tier))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(tier_rgb(tier))
        };
        spans.push(Span::styled(format!(" P{tier} {count} "), style));
        spans.push(Span::raw(" "));
    }
    spans
}

fn draw_outline(f: &mut ratatui::Frame, app: &App, area: Rect) {
    if !app.outline_visible {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(col::BORDER))
        .title(" Outline ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);
    f.render_widget(Paragraph::new(Line::from(tier_chips(app))), rows[0]);

    let mut items: Vec<ListItem<'static>> = Vec::new();
    // selectable[i] = outline index of the hunk on row i.
    let mut selectable: Vec<usize> = Vec::new();
    for oi in 0..app.len() {
        if !app.visible(oi) {
            continue;
        }
        let gr = &app.groups[oi];
        let label_h = app.label_entry(oi);
        let tier = gr.priority.as_u8();
        let depth = outline_depth(&app.parents, oi).min(3);
        let indent = "  ".repeat(depth);
        let twig = if depth > 0 { "└ " } else { "" };
        let viewed = app.inner.viewed.contains(&gr.identity());
        let check = if viewed { " ✓" } else { "" };
        let n_comments = app.card_comments(oi).len();
        let dot = if n_comments > 0 {
            format!(" ◆{n_comments}")
        } else {
            String::new()
        };
        let base = if viewed {
            Style::default().fg(col::FG_DIM).add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(col::FG)
        };
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!("  {indent}{twig}"),
                Style::default().fg(col::FG_DIM),
            ),
            Span::styled(
                format!("P{tier}"),
                Style::default()
                    .fg(tier_rgb(tier))
                    .add_modifier(Modifier::BOLD),
            ),
            // ✓/◆ right after the badge: labels truncate these off on
            // narrow panes, so indicators come first.
            Span::styled(format!("{check}{dot} "), Style::default().fg(col::ACCENT)),
            Span::styled(
                format!("{}  {}:{}", label_h.label, gr.file, gr.span.0),
                base,
            ),
        ])));
        selectable.push(oi);
    }
    if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "  (no hunks in filter)",
            Style::default().fg(col::FG_DIM),
        ))));
    }
    let row = selectable
        .iter()
        .position(|&v| v == app.selected)
        .unwrap_or(0);
    let mut state = ListState::default();
    state.select(Some(row));
    let list = List::new(items)
        .highlight_style(Style::default().bg(col::PANEL2))
        .highlight_symbol("» ");
    f.render_stateful_widget(list, rows[1], &mut state);
}

/// Word-wrap to `width` (chars), keeping at least one line. Rationale and
/// comment bodies are the only wrapped content — body code truncates.
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = Vec::new();
    for raw in s.lines() {
        let mut line = String::new();
        for word in raw.split_whitespace() {
            let need = if line.is_empty() {
                word.chars().count()
            } else {
                line.chars().count() + 1 + word.chars().count()
            };
            if need > width && !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn relative_age(now: u64, ts: u64) -> String {
    let d = now.saturating_sub(ts);
    if d < 60 {
        "just now".into()
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86400)
    }
}

/// Assemble the detail column: one card per visible hunk, comment rows
/// under their card. Also fills app.card_spans
/// and app.content for scroll bookkeeping, then adjusts detail_scroll.
fn draw_detail(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(col::BORDER))
        .title(" Detail ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let width = inner.width.max(20);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<(usize, u16, u16)> = Vec::new();

    let now = state::now_unix();
    let mut row: u16 = lines.len() as u16;
    for oi in 0..app.len() {
        if !app.visible(oi) {
            continue;
        }
        let start = row;
        let gr = &app.groups[oi];
        let anchor = app.anchor(oi);
        let label_h = app.label_entry(oi);
        let tier = gr.priority.as_u8();
        let viewed = app.inner.viewed.contains(&gr.identity());
        let collapsed = app.collapsed.contains(&oi);
        let focused = oi == app.selected;
        let comments = app.card_comments(oi);
        let head_bg = if focused {
            Style::default().bg(col::PANEL2)
        } else {
            Style::default()
        };

        let mut head = vec![
            Span::styled(
                if collapsed { " ▸ " } else { " ▾ " },
                Style::default().fg(col::FG_DIM),
            ),
            Span::styled(
                format!(" P{tier} "),
                Style::default()
                    .fg(Color::Black)
                    .bg(tier_rgb(tier))
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        // ✓/◆ sit right after the badge so long labels cannot clip them.
        if viewed {
            head.push(Span::styled(" ✓ viewed", Style::default().fg(col::ACCENT)));
        }
        if !comments.is_empty() {
            head.push(Span::styled(
                format!(" ◆{}", comments.len()),
                Style::default().fg(col::ACCENT),
            ));
        }
        head.push(Span::styled(
            format!(" {}", label_h.label),
            Style::default().fg(col::FG).add_modifier(Modifier::BOLD),
        ));
        head.push(Span::styled(
            format!("  {}:{}–{}", gr.file, gr.span.0, gr.span.1),
            Style::default().fg(col::FG_DIM),
        ));
        if !matches!(anchor.confidence, Confidence::High) {
            head.push(Span::styled(
                format!(" ({:?})", anchor.confidence).to_lowercase(),
                Style::default().fg(col::FG_DIM),
            ));
        }
        lines.push(Line::from(head).style(head_bg));
        row += 1;

        if !collapsed {
            lines.push(Line::from(Span::styled(
                "─".repeat(width as usize),
                Style::default().fg(col::BORDER),
            )));
            row += 1;
            if !label_h.rationale.is_empty() {
                for w in wrap_text(&label_h.rationale, width.saturating_sub(2) as usize) {
                    lines.push(Line::from(Span::styled(
                        format!(" {w}"),
                        Style::default()
                            .fg(col::FG_DIM)
                            .add_modifier(Modifier::ITALIC),
                    )));
                    row += 1;
                }
                lines.push(Line::raw(""));
                row += 1;
            }

            // Body rows with gutter + pin marker.
            let pins = row_pins_for(&app.inner, &app.file_key(oi));
            for r in &app.bodies[oi] {
                let pinned = row_pinned(r, &pins);
                let gutter_style = if pinned {
                    Style::default()
                        .fg(col::ACCENT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(col::FG_DIM)
                };
                let bg = match r.kind {
                    RowKind::Add => Some(col::ADD_BG),
                    RowKind::Del => Some(col::DEL_BG),
                    _ => None,
                };
                let mut spans: Vec<Span<'static>> = vec![
                    Span::styled(if pinned { "▍" } else { " " }, gutter_style),
                    Span::styled(format!("{:>4} ", r.gutter), gutter_style),
                    Span::styled(
                        match r.kind {
                            RowKind::Add => "+",
                            RowKind::Del => "-",
                            RowKind::Warn => "\\",
                            RowKind::Ctx => " ",
                        },
                        Style::default().fg(col::FG_DIM),
                    ),
                ];
                for (text, fg) in &r.spans {
                    spans.push(Span::styled(text.clone(), Style::default().fg(*fg)));
                }
                let mut line = Line::from(spans);
                if let Some(bg) = bg {
                    line = line.style(Style::default().bg(bg));
                }
                lines.push(line);
                row += 1;
            }

            // Comment rows under the card; ←/→ choose the target of x/d.
            for (ci, c) in comments.iter().enumerate() {
                let chosen = focused && ci == app.comment_cursor;
                let loc = match c.end_row.filter(|&e| e > c.row) {
                    Some(e) => format!("{}:{}–{}", c.file, c.row, e),
                    None => format!("{}:{}", c.file, c.row),
                };
                let mut body_first = c.body.lines().next().unwrap_or("").to_string();
                if c.body.lines().nth(1).is_some() {
                    body_first.push('…');
                }
                let style = if chosen {
                    Style::default().bg(col::PANEL2)
                } else {
                    Style::default()
                };
                lines.push(
                    Line::from(vec![
                        Span::styled(
                            if chosen { " ▶× " } else { "  × " },
                            Style::default().fg(if chosen {
                                Color::Rgb(0xf8, 0x51, 0x49)
                            } else {
                                col::FG_DIM
                            }),
                        ),
                        Span::styled(loc, Style::default().fg(col::FG_DIM)),
                        Span::styled(
                            format!(" · {} · {}", c.author, relative_age(now, c.ts_unix)),
                            Style::default().fg(col::FG_DIM),
                        ),
                        Span::raw("  "),
                        Span::styled(body_first, Style::default().fg(col::FG)),
                    ])
                    .style(style),
                );
                row += 1;
            }
            lines.push(Line::raw(""));
            row += 1;
        }
        spans.push((oi, start, row));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            " (no hunks visible — loosen the tier filter with > or F)",
            Style::default().fg(col::FG_DIM),
        )));
        row += 1;
    }

    app.viewport = inner.height;
    app.content = row.max(lines.len() as u16);
    app.card_spans = spans;
    if app.follow {
        if let Some(&(_, s, e)) = app
            .card_spans
            .iter()
            .find(|&&(oi, _, _)| oi == app.selected)
        {
            app.detail_scroll = ensure_visible(app.detail_scroll, app.viewport, s, e);
        } else {
            app.detail_scroll = 0;
        }
        app.follow = false;
    }
    let max_scroll = app.content.saturating_sub(app.viewport);
    app.detail_scroll = app.detail_scroll.min(max_scroll);

    let p = Paragraph::new(lines).scroll((app.detail_scroll, 0));
    f.render_widget(p, inner);
}

fn draw_footer(f: &mut ratatui::Frame, app: &App, area: Rect) {
    if let Some(comp) = &app.composer {
        let h = app.label_entry(comp.oi);
        let loc = match comp.end_row.filter(|&e| e > comp.row) {
            Some(e) => format!("{}:{}–{}", comp.file, comp.row, e),
            None => format!("{}:{}", comp.file, comp.row),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(col::ACCENT))
            .title(format!(" comment on {loc} — {} ", h.label));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let draft_lines = inner.height.saturating_sub(1) as usize;
        let all: Vec<&str> = comp.buffer.lines().collect();
        let tail: Vec<&str> = all
            .iter()
            .skip(all.len().saturating_sub(draft_lines.max(1)))
            .copied()
            .collect();
        let mut lines: Vec<Line> = tail.iter().map(|l| Line::raw(l.to_string())).collect();
        match lines.last_mut() {
            Some(last) => last
                .spans
                .push(Span::styled("▌", Style::default().fg(col::ACCENT))),
            None => lines.push(Line::from(Span::styled(
                "▌",
                Style::default().fg(col::ACCENT),
            ))),
        }
        lines.push(Line::from(Span::styled(
            " Ctrl+Enter / Ctrl+S save · Enter newline · Esc cancel",
            Style::default().fg(col::FG_DIM),
        )));
        f.render_widget(Paragraph::new(lines), inner);
        return;
    }

    let notice = app
        .status
        .as_ref()
        .filter(|(t, _)| t.elapsed() < Duration::from_secs(4))
        .map(|(_, m)| m.as_str());
    let text = notice.map(|m| format!(" {m}")).unwrap_or_else(|| {
        " j/k move · J/K page · v viewed · e editor · c comment · ←/→+x/d comment · t fold · f outline · </>/F/1-8 tier · ? keys · q quit"
            .to_string()
    });
    let style = if notice.is_some() {
        Style::default().fg(col::ACCENT)
    } else {
        Style::default().fg(col::FG_DIM)
    };
    f.render_widget(Paragraph::new(text).style(style), area);
}

/// Composer panel height: borders + draft tail (≤4 lines) + hint row.
fn composer_height(app: &App, _width: u16) -> u16 {
    match &app.composer {
        None => 1,
        Some(c) => {
            let draft = c.buffer.lines().count().clamp(1, 4) as u16;
            2 + draft + 1
        }
    }
}

fn draw_help(f: &mut ratatui::Frame, area: Rect) {
    let w = HELP_TEXT
        .lines()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(40) as u16
        + 4;
    let h = HELP_TEXT.lines().count() as u16 + 2;
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    let popup = Rect::new(
        area.x + (area.width.saturating_sub(w)) / 2,
        area.y + (area.height.saturating_sub(h)) / 2,
        w,
        h,
    );
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(col::ACCENT))
        .title(" keys — ? / Esc closes ");
    f.render_widget(Paragraph::new(HELP_TEXT).block(block), popup);
}

/// A body row is pinned when its diff row is inside a commented range.
fn row_pinned(r: &BodyRow, pins: &[(u32, u32)]) -> bool {
    let Some(row) = r.row else { return false };
    pins.iter().any(|&(lo, hi)| lo <= row && row <= hi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use adhdiff_shared::Priority;
    use std::path::PathBuf;

    fn fixture_map() -> AdhDiffMap {
        AdhDiffMap {
            title: "test map".into(),
            summary: "summary".into(),
            hunks: vec![
                LabeledHunk {
                    file: PathBuf::from("src/sample.rs"),
                    row_start: 6,
                    row_end: 9,
                    priority: Priority::P0,
                    label: "core change".into(),
                    rationale: "why".into(),
                    mechanical: false,
                    confidence: Confidence::High,
                    supports: None,
                },
                LabeledHunk {
                    file: PathBuf::from("src/sample.rs"),
                    row_start: 11,
                    row_end: 14,
                    priority: Priority::P1,
                    label: "switch call site".into(),
                    rationale: String::new(),
                    mechanical: false,
                    confidence: Confidence::Medium,
                    supports: Some(6),
                },
                LabeledHunk {
                    file: PathBuf::from("docs/usage.md"),
                    row_start: 21,
                    row_end: 22,
                    priority: Priority::P4,
                    label: "usage docs".into(),
                    rationale: String::new(),
                    mechanical: false,
                    confidence: Confidence::High,
                    supports: None,
                },
            ],
            diff_sha256: String::new(),
        }
    }

    const FIXTURE_DIFF: &str = "\
diff --git a/src/sample.rs b/src/sample.rs
index 1111111..2222222 100644
--- a/src/sample.rs
+++ b/src/sample.rs
@@ -10,3 +10,4 @@
 fn existing() {
     let x = 1;
+    let y = 2;
 }
@@ -20,3 +21,3 @@
 fn other() {
-    call_old();
+    call_new();
 }
diff --git a/docs/usage.md b/docs/usage.md
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/docs/usage.md
@@ -0,0 +1,2 @@
+# usage
+words
";

    /// App over a unique temp sidecar dir; cleaned up by callers that
    /// persist. The layout mirrors what run_tui builds via state::load.
    fn fixture_app(tag: &str) -> (App, PathBuf) {
        let files = parse_unified(FIXTURE_DIFF.as_bytes()).unwrap();
        let map = fixture_map();
        let dir =
            std::env::temp_dir().join(format!("adhdiff-tui-test-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let inner = Inner {
            map,
            files,
            comments: Default::default(),
            viewed: Default::default(),
            state_path: dir.join("state.json"),
            diff_sha: "abcdef1234567890".into(),
            workdir_hint: None,
            editor: None,
            body_cache: Default::default(),
        };
        (App::new(inner), dir)
    }

    #[test]
    fn root_keys_cover_parity_set() {
        use KeyCode::Char as C;
        let m = KeyModifiers::NONE;
        assert_eq!(root_action(C('q'), m), Some(Action::Quit));
        assert_eq!(root_action(C('j'), m), Some(Action::Move(1)));
        assert_eq!(root_action(C('k'), m), Some(Action::Move(-1)));
        assert_eq!(root_action(C('n'), m), Some(Action::Move(1)));
        assert_eq!(root_action(C('p'), m), Some(Action::Move(-1)));
        assert_eq!(root_action(KeyCode::Down, m), Some(Action::Move(1)));
        assert_eq!(root_action(C('J'), m), Some(Action::PageScroll(1)));
        assert_eq!(root_action(C('K'), m), Some(Action::PageScroll(-1)));
        assert_eq!(root_action(C('<'), m), Some(Action::StepFilter(-1)));
        assert_eq!(root_action(C('>'), m), Some(Action::StepFilter(1)));
        assert_eq!(root_action(C('F'), m), Some(Action::CycleFilter));
        assert_eq!(root_action(C('1'), m), Some(Action::FilterTier(0)));
        assert_eq!(root_action(C('8'), m), Some(Action::FilterTier(7)));
        assert_eq!(root_action(C('v'), m), Some(Action::ToggleViewed));
        assert_eq!(root_action(C('e'), m), Some(Action::OpenEditor));
        assert_eq!(root_action(C('c'), m), Some(Action::OpenComposer));
        assert_eq!(root_action(C('t'), m), Some(Action::ToggleCollapsed));
        assert_eq!(root_action(C('f'), m), Some(Action::ToggleOutline));
        assert_eq!(root_action(C('?'), m), Some(Action::ToggleHelp));
        assert_eq!(root_action(C('x'), m), Some(Action::DeleteComment));
        assert_eq!(root_action(C('d'), m), Some(Action::DeleteComment));
        assert_eq!(root_action(KeyCode::Esc, m), Some(Action::CloseOverlay));
        // Ctrl+C quits; other ctrl combos are not letter bindings.
        assert_eq!(
            root_action(C('c'), KeyModifiers::CONTROL),
            Some(Action::Quit)
        );
        assert_eq!(root_action(C('j'), KeyModifiers::CONTROL), None);
        assert_eq!(root_action(C('9'), m), None);
    }

    #[test]
    fn composer_keys_edit_without_losing_draft() {
        use KeyCode::Char as C;
        let m = KeyModifiers::NONE;
        assert_eq!(
            composer_key(KeyCode::Enter, KeyModifiers::CONTROL),
            ComposerKey::Submit
        );
        assert_eq!(
            composer_key(C('s'), KeyModifiers::CONTROL),
            ComposerKey::Submit
        );
        assert_eq!(composer_key(KeyCode::Enter, m), ComposerKey::Newline);
        assert_eq!(composer_key(KeyCode::Esc, m), ComposerKey::Cancel);
        assert_eq!(composer_key(KeyCode::Backspace, m), ComposerKey::Backspace);
        assert_eq!(composer_key(C('x'), m), ComposerKey::Push('x'));
        assert_eq!(composer_key(C('?'), m), ComposerKey::Push('?'));
        // Ctrl/Alt-modified chars never edit the draft.
        assert_eq!(
            composer_key(C('c'), KeyModifiers::CONTROL),
            ComposerKey::Ignore
        );
        assert_eq!(composer_key(KeyCode::Left, m), ComposerKey::Ignore);
    }

    #[test]
    fn ensure_visible_moves_minimally() {
        // Inside: unchanged.
        assert_eq!(ensure_visible(2, 10, 3, 6), 2);
        // Above: scroll up to the card start.
        assert_eq!(ensure_visible(4, 10, 1, 3), 1);
        // Below: scroll down just enough to reveal the end.
        assert_eq!(ensure_visible(0, 10, 14, 18), 8);
        // Taller than viewport: show the start (card header).
        assert_eq!(ensure_visible(0, 10, 2, 20), 2);
    }

    #[test]
    fn tier_walk_steps_and_cycles_present_tiers() {
        let tiers = vec![0u8, 1, 4];
        assert_eq!(step_tier(&tiers, 4, -1), Some(1));
        assert_eq!(step_tier(&tiers, 0, -1), Some(0));
        assert_eq!(step_tier(&tiers, 1, 1), Some(4));
        assert_eq!(cycle_tier(&tiers, 4), Some(0));
        assert_eq!(cycle_tier(&tiers, 0), Some(1));
        assert_eq!(step_tier(&[], 2, 1), None);
        assert_eq!(cycle_tier(&[], 2), None);
    }

    #[test]
    fn filter_to_tier_hides_and_normalizes_selection() {
        let (mut app, dir) = fixture_app("filter");
        // Fixture reading order: root P0, its P1 child, P4 root.
        assert!(!app.visible(2) || app.max_tier == 4);
        app.filter_to_tier(0);
        assert_eq!(app.max_tier, 0);
        assert!(app.visible(0) && !app.visible(2));
        assert_eq!(app.selected, 0);
        // Selection on a hidden hunk snaps back to a visible one.
        app.selected = 2;
        app.normalize_selection();
        assert!(app.visible(app.selected));
        app.set_tier_filter(4);
        assert!(app.visible(2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn viewed_toggle_collapses_and_persists_sidecar() {
        let (mut app, dir) = fixture_app("viewed");
        app.toggle_viewed(0);
        assert!(app.inner.viewed.contains("src/sample.rs:6"));
        assert!(app.collapsed.contains(&0));
        // Marking viewed advances to the next visible hunk (gui parity).
        assert_eq!(app.selected, 1);

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&app.inner.state_path).unwrap()).unwrap();
        assert_eq!(json["@viewed"][0], "src/sample.rs:6");

        app.toggle_viewed(0);
        assert!(!app.inner.viewed.contains("src/sample.rs:6"));
        assert!(!app.collapsed.contains(&0));
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&app.inner.state_path).unwrap()).unwrap();
        assert_eq!(json["@viewed"].as_array().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn composer_save_delete_roundtrip_matches_sidecar_shape() {
        let (mut app, dir) = fixture_app("comment");
        // c on the selected hunk anchors at its first row.
        app.open_composer(0);
        let comp = app.composer.as_ref().unwrap();
        assert_eq!(comp.file, "src/sample.rs");
        assert_eq!(comp.row, 6);
        app.composer.as_mut().unwrap().buffer = "first note".into();
        app.submit_composer();

        let key = comment_key("src/sample.rs", 6, None);
        let cs = &app.inner.comments[&key];
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].author, "human");
        assert_eq!(cs[0].label_at_time, "core change");
        assert_eq!(cs[0].body, "first note");

        // Sidecar carries exactly what serve/gui read back.
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&app.inner.state_path).unwrap()).unwrap();
        assert_eq!(json[key.as_str()][0]["body"], "first note");
        assert_eq!(json[key.as_str()][0]["author"], "human");

        // Saving a second comment on the same line replaces in place.
        app.open_composer(0);
        assert_eq!(app.composer.as_ref().unwrap().buffer, "first note");
        app.composer.as_mut().unwrap().buffer = "rewritten".into();
        app.submit_composer();
        assert_eq!(app.inner.comments.values().flatten().count(), 1);
        assert_eq!(app.inner.comments[&key][0].body, "rewritten");

        // ←/→ + x: delete the chosen row; empty keys vacate the map.
        app.comment_cursor = 0;
        app.delete_selected_comment();
        assert_eq!(app.inner.comments.values().flatten().count(), 0);
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&app.inner.state_path).unwrap()).unwrap();
        assert!(json.get(key.as_str()).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn saved_comment(file: &str, row: u32, body: &str) -> SavedComment {
        SavedComment {
            id: state::short_id(),
            file: file.into(),
            row,
            end_row: None,
            label_at_time: "core change".into(),
            author: "human".into(),
            body: body.into(),
            ts_unix: state::now_unix(),
        }
    }

    #[test]
    fn restore_bytes_are_exact_and_ordered() {
        let mut buf = Vec::new();
        write_restore(&mut buf);
        assert_eq!(buf, b"\x1b[<1u\x1b[?1049l\x1b[?25h");
    }

    #[test]
    fn empty_map_keys_do_not_panic() {
        let (mut app, dir) = fixture_app("empty");
        app.inner.map.hunks.clear();
        app.groups.clear();
        app.bodies.clear();
        let codes = [
            KeyCode::Char('x'),
            KeyCode::Char('d'),
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Char('e'),
            KeyCode::Char('c'),
            KeyCode::Char('t'),
            KeyCode::Char('v'),
            KeyCode::Char('j'),
            KeyCode::Char('k'),
        ];
        for code in codes {
            handle_key(&mut app, code, KeyModifiers::NONE);
        }
        assert!(!app.quit);
        assert!(app.composer.is_none());
        assert_eq!(app.selected, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_failure_survives_flash_ordering() {
        let (mut app, dir) = fixture_app("persistfail");
        // Removing the sidecar dir makes persist_sidecar's write fail.
        std::fs::remove_dir_all(&dir).unwrap();
        app.toggle_viewed(0);
        let msg = &app.status.as_ref().unwrap().1;
        assert!(
            msg.starts_with("persist failed"),
            "expected persist failure flash, got: {msg}"
        );
        // comment path must not mask it either
        app.open_composer(0);
        app.composer.as_mut().unwrap().buffer = "note".into();
        app.submit_composer();
        let msg = &app.status.as_ref().unwrap().1;
        assert!(msg.starts_with("persist failed"), "got: {msg}");
    }

    #[test]
    fn stacked_comments_delete_reports_count() {
        let (mut app, dir) = fixture_app("stacked");
        let key = comment_key("src/sample.rs", 6, None);
        app.inner.comments.insert(
            key.clone(),
            vec![
                saved_comment("src/sample.rs", 6, "one"),
                saved_comment("src/sample.rs", 6, "two"),
            ],
        );
        assert_eq!(app.card_comments(0).len(), 2);
        app.comment_cursor = 0;
        app.delete_selected_comment();
        assert_eq!(app.status.as_ref().unwrap().1, "deleted 2 comments");
        assert!(!app.inner.comments.contains_key(&key));
        // sidecar no longer carries the key
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&app.inner.state_path).unwrap()).unwrap();
        assert!(json.get(key.as_str()).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn editor_jump_maps_the_hunk_row_to_a_new_file_line() {
        let (mut app, dir) = fixture_app("editor");
        app.inner.workdir_hint = Some(dir.clone());
        // Card 1 is the second sample.rs hunk: its first row is the
        // context line `fn other()`, new-side line 21.
        app.request_editor(1);
        assert_eq!(app.pending_editor, Some(("src/sample.rs".to_string(), 21)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn body_rows_carry_diff_rows_and_pins_cover_deletions() {
        let (app, dir) = fixture_app("pins");
        let rows: Vec<Option<u32>> = app.bodies[1].iter().map(|r| r.row).collect();
        assert_eq!(rows, vec![Some(11), Some(12), Some(13), Some(14)]);
        let del = app.bodies[1]
            .iter()
            .find(|r| r.kind == RowKind::Del)
            .unwrap();
        assert!(row_pinned(del, &[(12, 12)]));
        assert!(!row_pinned(del, &[(13, 14)]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn windowed_editor_whitelist() {
        for stem in [
            "zed",
            "zeditor",
            "subl",
            "code",
            "code-insiders",
            "codium",
            "fleet",
        ] {
            assert!(editor_is_windowed(stem), "{stem} should spawn detached");
        }
        for stem in [
            "vi",
            "vim",
            "nvim",
            "nano",
            "emacs",
            "emacsclient",
            "helix",
            "hx",
            "kak",
            "mystery-ed",
        ] {
            assert!(!editor_is_windowed(stem), "{stem} should take the terminal");
        }
    }

    #[test]
    fn segmented_map_renders_one_card_per_raw_hunk() {
        // The segment-tiled fixture has 6 map entries across 3 raw
        // hunks: the cards must follow the raw hunks, and each card's
        // body covers its whole raw hunk exactly once.
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/segments");
        let map: AdhDiffMap =
            serde_json::from_slice(&std::fs::read(format!("{root}/demo.map.json")).unwrap())
                .unwrap();
        let diff = std::fs::read(format!("{root}/demo.diff")).unwrap();
        let files = parse_unified(&diff).unwrap();
        let dir =
            std::env::temp_dir().join(format!("adhdiff-tui-test-segments-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let inner = Inner {
            map,
            files,
            comments: Default::default(),
            viewed: Default::default(),
            state_path: dir.join("state.json"),
            diff_sha: "abcdef1234567890".into(),
            workdir_hint: None,
            editor: None,
            body_cache: Default::default(),
        };
        let map_entries = inner.map.hunks.len();
        let app = App::new(inner);
        assert_eq!(map_entries, 6);
        // 3 cards, not 6: reading order follows the derived tier.
        assert_eq!(app.len(), 3);
        assert_eq!(app.bodies.len(), 3);
        assert_eq!(app.groups[0].file, "src/big.rs");
        assert_eq!(app.groups[0].priority, Priority::P1);
        assert_eq!(
            app.label_entry(0).label,
            "hand-written lookup: binary search plus bucket check"
        );
        // The 180-line new-file body renders once behind the anchor.
        assert_eq!(app.groups[0].identity(), "src/big.rs:6");
        assert_eq!(app.bodies[0].len(), 180);
        // lib.rs comes next (P2), then the degenerate CHANGELOG (P4).
        assert_eq!(app.groups[1].file, "src/lib.rs");
        assert_eq!(app.groups[1].priority, Priority::P2);
        assert_eq!(app.groups[2].file, "docs/CHANGELOG.md");
        assert_eq!(app.groups[2].priority, Priority::P4);
        assert_eq!(app.groups[2].members.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
