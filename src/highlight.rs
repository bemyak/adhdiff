//! Tree-sitter syntax highlighting shared by the hunk-HTML builder (Tauri
//! UI) and the TUI. A raw hunk is split into an old-side and a new-side
//! virtual document, each parsed once; spans are reported per body row.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter as TsHighlighter};
use tree_sitter_language_pack::{
    detect_language, get_highlights_query, get_injections_query, get_language, get_locals_query,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Keyword,
    String,
    Comment,
    Function,
    Type,
    Number,
    Constant,
    Variable,
    Property,
    Operator,
    Punctuation,
    Attribute,
    Tag,
    Label,
}

impl Class {
    pub fn name(self) -> &'static str {
        match self {
            Class::Keyword => "keyword",
            Class::String => "string",
            Class::Comment => "comment",
            Class::Function => "function",
            Class::Type => "type",
            Class::Number => "number",
            Class::Constant => "constant",
            Class::Variable => "variable",
            Class::Property => "property",
            Class::Operator => "operator",
            Class::Punctuation => "punctuation",
            Class::Attribute => "attribute",
            Class::Tag => "tag",
            Class::Label => "label",
        }
    }
}

/// `(byte_start, byte_end, class)` within one row's text.
pub type Span = (usize, usize, Class);

const PREFIXES: &[(&str, Class)] = &[
    ("keyword", Class::Keyword),
    ("string", Class::String),
    ("comment", Class::Comment),
    ("function", Class::Function),
    ("type", Class::Type),
    ("number", Class::Number),
    ("constant", Class::Constant),
    ("variable", Class::Variable),
    ("property", Class::Property),
    ("operator", Class::Operator),
    ("punctuation", Class::Punctuation),
    ("attribute", Class::Attribute),
    ("tag", Class::Tag),
    ("label", Class::Label),
    ("boolean", Class::Constant),
    ("float", Class::Number),
    ("integer", Class::Number),
    ("constructor", Class::Type),
    ("escape", Class::String),
    ("module", Class::Type),
    ("namespace", Class::Type),
    ("text.title", Class::Keyword),
    ("text.literal", Class::String),
    ("text.uri", Class::String),
    ("text.reference", Class::String),
];

/// Class of a capture name by its longest matching dotted prefix.
fn class_of(capture: &str) -> Option<Class> {
    PREFIXES
        .iter()
        .filter(|(p, _)| {
            capture == *p || capture.strip_prefix(p).is_some_and(|r| r.starts_with('.'))
        })
        .max_by_key(|(p, _)| p.len())
        .map(|&(_, c)| c)
}

struct Config {
    ts: HighlightConfiguration,
    classes: Vec<Option<Class>>,
}

fn warn_once(lang: &str, msg: &str) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    if seen.lock().is_ok_and(|mut s| s.insert(lang.to_string())) {
        eprintln!("adhdiff: highlighting disabled for {lang}: {msg}");
    }
}

fn build_config(lang: &str) -> Option<Config> {
    let language = get_language(lang).ok()?;
    let highlights = get_highlights_query(lang)?;
    let mut ts = match HighlightConfiguration::new(
        language,
        lang,
        highlights,
        get_injections_query(lang).unwrap_or(""),
        get_locals_query(lang).unwrap_or(""),
    ) {
        Ok(c) => c,
        Err(e) => {
            warn_once(lang, &e.to_string());
            return None;
        }
    };
    let names: Vec<String> = ts
        .query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    ts.configure(&names);
    let classes = names.iter().map(|n| class_of(n)).collect();
    Some(Config { ts, classes })
}

fn config_for(lang: &str) -> Option<&'static Config> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<&'static Config>>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().ok()?;
    *cache
        .entry(lang.to_string())
        .or_insert_with(|| build_config(lang).map(|c| &*Box::leak(Box::new(c))))
}

/// Spans over `doc` as `(start, end, class)` in document byte offsets.
fn highlight_doc(lang: &'static str, doc: &str) -> Vec<Span> {
    let Some(cfg) = config_for(lang) else {
        return Vec::new();
    };
    let mut hl = TsHighlighter::new();
    let events = match hl.highlight(&cfg.ts, doc.as_bytes(), None, None, |name| {
        config_for(name).map(|c| &c.ts)
    }) {
        Ok(e) => e,
        Err(e) => {
            warn_once(lang, &e.to_string());
            return Vec::new();
        }
    };
    let mut stack: Vec<Option<Class>> = Vec::new();
    let mut out: Vec<Span> = Vec::new();
    for ev in events {
        match ev {
            Ok(HighlightEvent::HighlightStart(h)) => {
                stack.push(cfg.classes.get(h.0).copied().flatten());
            }
            Ok(HighlightEvent::HighlightEnd) => {
                stack.pop();
            }
            Ok(HighlightEvent::Source { start, end }) => {
                if let Some(class) = stack.iter().rev().find_map(|c| *c) {
                    out.push((start, end, class));
                }
            }
            Err(e) => {
                warn_once(lang, &e.to_string());
                return Vec::new();
            }
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Old,
    New,
    Both,
    Neither,
}

/// Side and code text (prefix char excluded) of one body row.
fn split_row(l: &str) -> (Side, &str) {
    if let Some(rest) = l.strip_prefix('+') {
        (Side::New, rest)
    } else if let Some(rest) = l.strip_prefix('-') {
        (Side::Old, rest)
    } else if l.starts_with('\\') || l.starts_with("@@") {
        (Side::Neither, l)
    } else {
        (Side::Both, l.strip_prefix(' ').unwrap_or(l))
    }
}

/// Per-row spans for one raw hunk body, indexed like `body.lines()`.
pub struct Highlighter {
    before: Vec<Vec<Span>>,
    rows: Vec<Vec<Span>>,
    after: Vec<Vec<Span>>,
}

impl Highlighter {
    #[cfg(test)]
    pub fn for_hunk(path: &str, body: &str) -> Self {
        Self::with_context(path, body, &[], &[])
    }

    /// Like `for_hunk`, with extra new-side lines directly above and below
    /// the body joining the new-side virtual document.
    pub fn with_context(path: &str, body: &str, before: &[&str], after: &[&str]) -> Self {
        let rows: Vec<(Side, &str)> = before
            .iter()
            .map(|t| (Side::New, *t))
            .chain(body.lines().map(split_row))
            .chain(after.iter().map(|t| (Side::New, *t)))
            .collect();
        let mut out = vec![Vec::new(); rows.len()];
        if let Some(lang) = detect_language(path) {
            let old = side_spans(lang, &rows, |s| matches!(s, Side::Old | Side::Both));
            let new = side_spans(lang, &rows, |s| matches!(s, Side::New | Side::Both));
            for (i, (side, _)) in rows.iter().enumerate() {
                match side {
                    Side::Old => out[i] = old[i].clone(),
                    Side::New | Side::Both => out[i] = new[i].clone(),
                    Side::Neither => {}
                }
            }
        }
        let tail = out.split_off(rows.len() - after.len());
        let body = out.split_off(before.len());
        Self {
            before: out,
            rows: body,
            after: tail,
        }
    }

    pub fn spans(&self, row: usize) -> &[Span] {
        self.rows.get(row).map_or(&[], Vec::as_slice)
    }

    pub fn before_spans(&self, i: usize) -> &[Span] {
        self.before.get(i).map_or(&[], Vec::as_slice)
    }

    pub fn after_spans(&self, i: usize) -> &[Span] {
        self.after.get(i).map_or(&[], Vec::as_slice)
    }
}

/// Highlight the virtual document of the rows `keep` selects; result is
/// indexed by original row (empty for rows outside the document).
fn side_spans(
    lang: &'static str,
    rows: &[(Side, &str)],
    keep: impl Fn(Side) -> bool,
) -> Vec<Vec<Span>> {
    let mut doc = String::new();
    let mut starts: Vec<(usize, usize)> = Vec::new();
    for (i, (side, text)) in rows.iter().enumerate() {
        if keep(*side) {
            starts.push((i, doc.len()));
            doc.push_str(text);
            doc.push('\n');
        }
    }
    let mut out = vec![Vec::new(); rows.len()];
    for (s, e, class) in highlight_doc(lang, &doc) {
        let first = starts.partition_point(|&(_, at)| at <= s).saturating_sub(1);
        for &(i, at) in &starts[first..] {
            let len = rows[i].1.len();
            if at >= e {
                break;
            }
            let (rs, re) = (s.max(at) - at, e.min(at + len) - at);
            if rs < re {
                match out[i].last_mut() {
                    Some((_, last_end, c)) if *last_end == rs && *c == class => *last_end = re,
                    _ => out[i].push((rs, re, class)),
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts<'a>(text: &'a str, spans: &[Span], class: Class) -> Vec<&'a str> {
        spans
            .iter()
            .filter(|s| s.2 == class)
            .map(|&(s, e, _)| &text[s..e])
            .collect()
    }

    #[test]
    fn capture_names_map_by_longest_prefix() {
        for (cap, class) in [
            ("function.method", Some(Class::Function)),
            ("keyword.control", Some(Class::Keyword)),
            ("string.special", Some(Class::String)),
            ("punctuation.bracket", Some(Class::Punctuation)),
            ("constant.builtin", Some(Class::Constant)),
            ("variable.parameter", Some(Class::Variable)),
            ("type.builtin", Some(Class::Type)),
            ("comment.documentation", Some(Class::Comment)),
            ("keywordish", None),
            ("boolean", Some(Class::Constant)),
            ("float", Some(Class::Number)),
            ("constructor", Some(Class::Type)),
            ("escape", Some(Class::String)),
            ("text.title", Some(Class::Keyword)),
            ("text.uri", Some(Class::String)),
            ("embedded", None),
            ("spell", None),
        ] {
            assert_eq!(class_of(cap), class, "{cap}");
        }
    }

    #[test]
    fn rust_rows_get_keyword_and_string_spans() {
        let body = "-let x = \"s\";\n+fn main() {\n";
        let h = Highlighter::for_hunk("src/a.rs", body);
        let del = "let x = \"s\";";
        assert_eq!(texts(del, h.spans(0), Class::Keyword), ["let"]);
        assert_eq!(texts(del, h.spans(0), Class::String), ["\"s\""]);
        let add = "fn main() {";
        assert_eq!(texts(add, h.spans(1), Class::Keyword), ["fn"]);
        assert!(h.spans(1).iter().all(|&(s, e, _)| e <= add.len() && s < e));
    }

    #[test]
    fn go_and_python_get_keyword_spans() {
        let go = Highlighter::for_hunk("m.go", "+func main() {\n+}\n");
        assert_eq!(
            texts("func main() {", go.spans(0), Class::Keyword),
            ["func"]
        );
        let py = Highlighter::for_hunk("m.py", "+def f():\n+    return 1\n");
        assert_eq!(texts("def f():", py.spans(0), Class::Keyword), ["def"]);
    }

    #[test]
    fn unknown_extension_and_grammarless_names_are_plain() {
        for path in [
            "notes.zzzunknown",
            "Makefile",
            "sub/dir/Makefile",
            "/dev/zero",
            "a.tf",
        ] {
            let h = Highlighter::for_hunk(path, "+fn main() {\n+all: x\n");
            assert!(h.spans(0).is_empty() && h.spans(1).is_empty(), "{path}");
        }
    }

    #[test]
    fn header_and_no_newline_rows_are_plain() {
        let body =
            "@@ -1,2 +1,2 @@ fn main() {\n-let a = 1;\n\\ No newline at end of file\n+let a = 2;\n";
        let h = Highlighter::for_hunk("a.rs", body);
        assert!(h.spans(0).is_empty());
        assert!(h.spans(2).is_empty());
        assert!(!h.spans(1).is_empty() && !h.spans(3).is_empty());
    }

    #[test]
    fn sides_with_different_line_counts_map_to_their_rows() {
        let body = " fn a() {\n-    let old = 1;\n-    let older = 2;\n+    let new = \"n\";\n }\n";
        let h = Highlighter::for_hunk("a.rs", body);
        let old_row = "    let older = 2;";
        let (s, e, _) = *h
            .spans(2)
            .iter()
            .find(|s| s.2 == Class::Keyword)
            .expect("keyword on second - row");
        assert_eq!(&old_row[s..e], "let");
        let new_row = "    let new = \"n\";";
        assert_eq!(texts(new_row, h.spans(3), Class::String), ["\"n\""]);
        assert!(h.spans(3).iter().all(|&(_, e, _)| e <= new_row.len()));
    }

    #[test]
    fn multibyte_row_spans_slice_on_char_boundaries() {
        let text = "let s = \"héllo → ✓\";";
        let h = Highlighter::for_hunk("a.rs", &format!("+{text}\n"));
        let strs = texts(text, h.spans(0), Class::String);
        assert_eq!(strs, ["\"héllo → ✓\""]);
    }

    #[test]
    fn context_rows_join_the_new_side_document() {
        let plain = Highlighter::for_hunk("a.rs", "+ fn x() {}\n");
        assert!(!texts(" fn x() {}", plain.spans(0), Class::Keyword).is_empty());
        let h = Highlighter::with_context("a.rs", "+ fn x() {}\n", &["/* open"], &["*/"]);
        assert!(texts(" fn x() {}", h.spans(0), Class::Keyword).is_empty());
        assert_eq!(
            texts(" fn x() {}", h.spans(0), Class::Comment),
            [" fn x() {}"]
        );
        assert_eq!(
            texts("/* open", h.before_spans(0), Class::Comment),
            ["/* open"]
        );
        assert_eq!(texts("*/", h.after_spans(0), Class::Comment), ["*/"]);
    }
}
