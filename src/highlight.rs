//! Shared syntect highlighting — used by the hunk-HTML builder (Tauri UI)
//! and the TUI so both colorize diff lines identically.
//!
//! Thread-local lazy syntect init; `FileHighlighter` is created per FILE
//! and called for each line in hunk order. Stateless ACROSS lines (a fresh
//! `HighlightLines` per line): multi-line strings/comments spanning hunks
//! don't carry state. Within a hunk body this is fine because lines are
//! highlighted in order through the same instance.

use syntect::highlighting::Color;

// Thread-local lazy syntect init — one ThemeSet/SyntaxSet per process.
thread_local! {
    static SYNTAX_SET: syntect::parsing::SyntaxSet =
        syntect::parsing::SyntaxSet::load_defaults_newlines();
    static THEME: syntect::highlighting::Theme = {
        let ts = syntect::highlighting::ThemeSet::load_defaults();
        ts.themes.get("base16-eighties.dark").cloned().unwrap_or_else(||
            ts.themes.values().next().cloned().unwrap_or_default()
        )
    };
}

fn lazy_static_syntect() {
    SYNTAX_SET.with(|_| ());
    THEME.with(|_| ());
}

/// Highlight one hunk, threading per-file state. Caller creates a fresh
/// `FileHighlighter` per FILE and calls it for each hunk in order; this
/// way stateful syntect parses don't restart mid-function.
pub struct FileHighlighter {
    syn: Option<syntect::parsing::SyntaxReference>,
}

impl FileHighlighter {
    /// Highlighter for one raw hunk: the first body line only informs
    /// first-line detection when the hunk starts at new line 1.
    pub fn for_hunk(path: &str, body: &str, new_start: u32) -> Self {
        let first_line = if new_start == 1 {
            body.lines().next().map_or("", |l| l.get(1..).unwrap_or(""))
        } else {
            ""
        };
        Self::for_path(path, first_line)
    }

    /// Syntax by file name, then extension of `path` (never opened: it
    /// comes from the diff), then `first_line`, else plain text.
    fn for_path(path: &str, first_line: &str) -> Self {
        lazy_static_syntect();
        let p = std::path::Path::new(path);
        let syn = SYNTAX_SET.with(|ss| {
            let by_name = p
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| ss.find_syntax_by_extension(n));
            let by_ext = || {
                p.extension()
                    .and_then(|e| e.to_str())
                    .and_then(|e| ss.find_syntax_by_extension(e))
            };
            by_name
                .or_else(by_ext)
                .or_else(|| ss.find_syntax_by_first_line(first_line))
                .cloned()
        });
        Self { syn }
    }

    /// Highlight one line as HTML spans (web renderer path).
    pub fn highlight_line_html(&self, line: &str) -> Option<String> {
        let s = self.syn.as_ref()?;
        use syntect::easy::HighlightLines;
        use syntect::html::{IncludeBackground, styled_line_to_highlighted_html};
        let theme = THEME.with(|t| t.clone());
        SYNTAX_SET.with(|ss| {
            let mut h = HighlightLines::new(s, &theme);
            h.highlight_line(line, ss)
                .ok()
                .and_then(|r| styled_line_to_highlighted_html(&r[..], IncludeBackground::No).ok())
        })
    }

    /// Highlight one line as `(text, rgb)` runs (gpui renderer path).
    /// The rgb is 0xRRGGBB; alpha and font styles are dropped — the line's
    /// background is the row's background in both UIs.
    pub fn highlight_line_runs(&self, line: &str) -> Option<Vec<(String, u32)>> {
        let s = self.syn.as_ref()?;
        use syntect::easy::HighlightLines;
        let theme = THEME.with(|t| t.clone());
        SYNTAX_SET.with(|ss| {
            let mut h = HighlightLines::new(s, &theme);
            let styled = h.highlight_line(line, ss).ok()?;
            let mut runs: Vec<(String, u32)> = Vec::new();
            for (style, text) in styled {
                let Color { r, g, b, .. } = style.foreground;
                let rgb = ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
                if let Some((last_text, last_rgb)) = runs.last_mut()
                    && *last_rgb == rgb
                {
                    last_text.push_str(text);
                    continue;
                }
                runs.push((text.to_string(), rgb));
            }
            Some(runs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(path: &str, first_line: &str) -> Option<String> {
        FileHighlighter::for_path(path, first_line)
            .syn
            .map(|s| s.name)
    }

    #[test]
    fn syntax_resolves_by_file_name_extension_then_first_line() {
        assert!(name("Makefile", "").is_some());
        assert!(name("sub/dir/Makefile", "").is_some());
        assert!(name("src/a.rs", "").is_some());
        assert_eq!(
            name("run", "#!/usr/bin/env python").as_deref(),
            Some("Python")
        );
        assert_eq!(name("notes.zzzunknown", "").as_deref(), None);
        assert!(name("/dev/zero", "").is_none());
    }

    #[test]
    fn first_line_only_counts_for_hunks_starting_at_line_one() {
        let body = "+#!/usr/bin/env python\n+print(1)\n";
        assert!(FileHighlighter::for_hunk("run", body, 1).syn.is_some());
        assert!(FileHighlighter::for_hunk("run", body, 7).syn.is_none());
    }
}
