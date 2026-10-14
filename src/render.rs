//! TTY rendering: the reviewable map.
//!
//! Reads an AdhDiffMap + the raw diff and produces a terminal view where
//! P0 sections come first, each RAW hunk gets one block (the outline
//! stays hunk-level: segments tile bodies, they don't become rows) with
//! a one-liner label and optional rationale, and the actual diff text
//! sits under the label.

use std::io::Write;

use crate::diff::{find_raw_for, parse_unified};
use adhdiff_shared::{AdhDiffMap, Priority};

/// NO_COLOR counts only when set to a non-empty value.
fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

/// Drop control characters (ESC and friends, `\r` included) so strings from
/// the map or the diff cannot drive the terminal; `\t` and `\n` stay.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|&c| c == '\t' || c == '\n' || !c.is_control())
        .collect()
}

/// ANSI codes. Keep minimal — if NO_COLOR is set we print plain.
fn bold(s: &str) -> String {
    if no_color() {
        s.to_string()
    } else {
        format!("\x1b[1m{s}\x1b[0m")
    }
}

fn dim(s: &str) -> String {
    if no_color() {
        s.to_string()
    } else {
        format!("\x1b[2m{s}\x1b[0m")
    }
}

fn color_for(p: Priority, s: &str) -> String {
    if no_color() {
        return s.to_string();
    }
    let code = match p {
        Priority::P0 => "1;31", // bright red — the gist
        Priority::P1 => "1;33", // yellow — supporting
        Priority::P2 => "1;36", // cyan — error handling
        _ => "37",              // dim white for the rest
    };
    format!("\x1b[{code}m{s}\x1b[0m")
}

/// Write the map to any writer. The files slice carries the raw diff
/// for inline display.
pub fn render_map<W: Write>(w: &mut W, map: &AdhDiffMap, diff: &[u8]) -> std::io::Result<()> {
    let files =
        parse_unified(diff).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    writeln!(w, "{}", bold("▍ Adhdiff"))?;
    writeln!(w, "  {}", bold(&sanitize(&map.title)))?;
    writeln!(w, "  {}", dim(&sanitize(&map.summary)))?;
    writeln!(w)?;

    // Header: tier histogram
    let counts = map.tier_counts();
    let hist: Vec<String> = counts
        .iter()
        .map(|(p, n)| format!("{}×{}", color_for(*p, &format!("P{}", p.as_u8())), n))
        .collect();
    writeln!(w, "  {}", hist.join(&dim("  ·  ")))?;
    writeln!(w)?;

    // Sections follow the group-level supports forest (one block per raw
    // hunk): each root in (derived tier, anchor) order, its subtree
    // depth-first right after it. Roots print flat as before; children
    // indent under their parent with a connector.
    let groups = crate::outline::raw_groups(map, &files);
    let parent: Vec<Option<usize>> = groups.iter().map(|g| g.parent).collect();

    for (n, gr) in groups.iter().enumerate() {
        let label_h = &map.hunks[gr.label_idx];
        // Depth in the supports tree; the step bound keeps a cyclic map
        // (a hard `check` error) from hanging the render.
        let mut depth = 0;
        let mut anc = n;
        while let Some(p) = parent[anc] {
            if depth >= groups.len() {
                break;
            }
            depth += 1;
            anc = p;
        }
        let connector = match parent[n] {
            None => String::new(),
            Some(p) if (0..groups.len()).rev().find(|&k| parent[k] == Some(p)) == Some(n) => {
                "└─ ".to_string()
            }
            Some(_) => "├─ ".to_string(),
        };
        let indent = "  ".repeat(depth);
        writeln!(
            w,
            "  {indent}{connector}{}  {}",
            color_for(gr.priority, &format!("[{}]", n + 1)),
            bold(&sanitize(&label_h.label))
        )?;
        // Continuation lines (rationale, location, body) align under
        // the label text: base padding + indent + connector width.
        let pad = " ".repeat(6 + 2 * depth + if depth > 0 { 3 } else { 0 });
        if !label_h.rationale.is_empty() {
            for line in sanitize(&label_h.rationale).lines() {
                writeln!(w, "{pad}{}", dim(line))?;
            }
        }
        writeln!(
            w,
            "{pad}{}",
            dim(&sanitize(&format!(
                "{} rows {}–{}",
                gr.file, gr.span.0, gr.span.1
            )))
        )?;
        if let Some((_, raw)) = find_raw_for(&files, gr.span.0) {
            for line in raw.body.lines().take(40).map(sanitize) {
                let prefix = if line.starts_with('+') {
                    "\x1b[32m"
                } else if line.starts_with('-') {
                    "\x1b[31m"
                } else {
                    "\x1b[2m"
                };
                let suffix = if no_color() { "" } else { "\x1b[0m" };
                writeln!(w, "{pad}{prefix}{line}{suffix}")?;
            }
        }
        writeln!(w)?;
    }

    // Footer: files table for quick orientation
    writeln!(w, "{}", bold("▍ Files"))?;
    for (path, prio) in map.files_by_importance() {
        let n: usize = map.hunks.iter().filter(|h| &h.file == path).count();
        writeln!(
            w,
            "  {} {:40}  {}",
            color_for(prio, &format!("P{}", prio.as_u8())),
            sanitize(&path.display().to_string()),
            dim(&format!("{n} hunk{s}", s = if n == 1 { "" } else { "s" }))
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_controls_but_keeps_tab_and_newline() {
        assert_eq!(sanitize("a\x1b[31mb\r\x07c\td\ne"), "a[31mbc\td\ne");
    }

    #[test]
    fn rendered_map_has_no_agent_supplied_escapes() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/segments");
        let mut map: AdhDiffMap =
            serde_json::from_slice(&std::fs::read(format!("{root}/demo.map.json")).unwrap())
                .unwrap();
        let diff = std::fs::read(format!("{root}/demo.diff")).unwrap();
        map.title = "t\x1b]0;pwn\x07".into();
        for h in &mut map.hunks {
            h.label = format!("{}\x1b[2J", h.label);
            h.rationale = "r\x1b[H".into();
        }
        let mut out = Vec::new();
        render_map(&mut out, &map, &diff).unwrap();
        let text = String::from_utf8(out).unwrap();
        for bad in ["\x07", "\x1b]", "\x1b[2J", "\x1b[H"] {
            assert!(!text.contains(bad), "{bad:?} reached the output");
        }
        assert!(text.contains("]0;pwn"));
    }
}
