//! The reviewer-agent prompt: the rubric template followed by the diff
//! inventory, every body row numbered with its diff row.

use crate::diff::FileDiff;

pub fn render_prompt_for(files: &[FileDiff]) -> String {
    let sections: Vec<String> = files
        .iter()
        .map(|f| {
            let name = if f.renamed {
                format!("{} → {}", f.old_path, f.new_path)
            } else {
                f.path().to_string()
            };
            let hunks: Vec<String> = f
                .hunks
                .iter()
                .map(|h| {
                    let width = h.row_end.to_string().len();
                    let mut lines = vec![format!(
                        "{:>width$} | {}",
                        h.row_start.saturating_sub(1),
                        h.header
                    )];
                    lines.extend(h.body.lines().enumerate().map(|(i, l)| {
                        let row = h
                            .row_start
                            .saturating_add(u32::try_from(i).unwrap_or(u32::MAX));
                        format!("{row:>width$} | {l}")
                    }));
                    let rows = if h.row_end >= h.row_start {
                        format!("rows {}–{}", h.row_start, h.row_end)
                    } else {
                        "no body rows".to_string()
                    };
                    format!(
                        "@@ {rows} (new:{}–{} old:{}–{})\n{}",
                        h.new_start,
                        h.new_end,
                        h.old_start,
                        h.old_end,
                        lines.join("\n")
                    )
                })
                .collect();
            format!("### {name}\n{}", hunks.join("\n"))
        })
        .collect();
    format!(
        "{}\n\n## Diff\n\n{}",
        include_str!("prompt_template.md"),
        sections.join("\n\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse_unified;

    #[test]
    fn every_body_row_is_numbered_and_nothing_is_truncated() {
        let body: String = (1..=60).map(|i| format!("+line {i}\n")).collect();
        let diff = format!(
            "diff --git a/f.rs b/f.rs\n--- /dev/null\n+++ b/f.rs\n@@ -0,0 +1,60 @@\n{body}"
        );
        let files = parse_unified(diff.as_bytes()).unwrap();
        let prompt = render_prompt_for(&files);
        assert!(prompt.contains("@@ rows 5–64 (new:1–60 old:0–0)"));
        assert!(prompt.contains(" 4 | @@ -0,0 +1,60 @@"));
        assert!(prompt.contains(" 5 | +line 1\n"));
        assert!(prompt.contains("64 | +line 60"));
        assert_eq!(prompt.matches(" | +line ").count(), 60);
    }

    #[test]
    fn gutter_numbers_match_the_diff_text() {
        let diff = "diff --git a/q.sql b/q.sql\n--- a/q.sql\n+++ b/q.sql\n@@ -1,2 +1,2 @@\n-- x\n++ y\n keep\n";
        let files = parse_unified(diff.as_bytes()).unwrap();
        let prompt = render_prompt_for(&files);
        for (i, line) in diff.lines().enumerate().skip(3) {
            assert!(
                prompt.contains(&format!("{} | {line}", i + 1)),
                "row {}",
                i + 1
            );
        }
    }
}
