//! Tree-sitter enrichment: for each hunk in the diff inventory, locate
//! the enclosing symbol in the NEW version of the file. Produced JSON
//! is what `adhdiff enrich` emits for the agent to label.
//!
//! Strategy (subagent recommendation):
//!   1. Parse new file with tree-sitter via tree-sitter-language-pack.
//!   2. named_descendant_for_point_range over the hunk's new-side range.
//!   3. Walk parents until kind matches a per-language "symbol" set.
//!   4. Fallback to the hunk-header text (git's xfuncname heuristic).
//!   5. Never fail — grammar missing/parse error → language "text".

use std::path::{Component, Path};
// The pack only re-exports `Language`; we add `tree-sitter` as a
// direct dep pinned to the version the pack uses (0.27) so the types
// interoperate.
use tree_sitter::{Node, Parser, Tree};
use tree_sitter_language_pack as tslp;

use crate::diff::{FileDiff, RawHunk};

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct EnrichedHunk {
    pub file: String,
    pub new_start: u32,
    pub new_end: u32,
    pub old_start: u32,
    pub old_end: u32,
    pub language: String,
    pub enclosing_symbol: Option<String>,
    pub enclosing_kind: Option<String>,
    pub hunk_header_context: String,
    /// First 30 lines of hunk body so the agent sees content inline.
    pub body_preview: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct EnrichedDiff {
    pub files: Vec<EnrichedFile>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct EnrichedFile {
    pub old_path: String,
    pub new_path: String,
    pub renamed: bool,
    pub language: String,
    pub hunks: Vec<EnrichedHunk>,
}

/// Map a file path to the language-pack's language name.
fn lang_for(path: &str) -> Option<&'static str> {
    tslp::detect_language(path)
}

/// Per-language node kinds that count as "symbols" (function, class,
/// method, type definition). Rough tables — the goal is name resolution
/// for the reviewer, not exactness. Anything not in the table falls
/// through to git-header context.
fn symbol_kinds(lang: &str) -> &'static [&'static str] {
    match lang {
        "rust" => &[
            "function_item",
            "impl_item",
            "struct_item",
            "enum_item",
            "trait_item",
            "mod_item",
            "type_item",
            "macro_definition",
        ],
        "go" => &[
            "function_declaration",
            "method_declaration",
            "type_declaration",
        ],
        "python" => &["function_definition", "class_definition"],
        "typescript" | "tsx" | "javascript" => &[
            "function_declaration",
            "generator_function_declaration",
            "class_declaration",
            "abstract_class_declaration",
            "method_definition",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
            "variable_declarator",
        ],
        "java" => &[
            "method_declaration",
            "class_declaration",
            "interface_declaration",
            "constructor_declaration",
        ],
        "c" | "cpp" => &[
            "function_definition",
            "struct_specifier",
            "class_specifier",
            "namespace_definition",
        ],
        "bash" => &["function_definition"],
        "nix" => &["binding", "attrset_expression", "let_expression"],
        "lua" => &["function_declaration", "function_definition"],
        "ruby" => &["method", "class", "module"],
        "haskell" => &["function", "data_declaration", "type_alias_declaration"],
        "elixir" => &["call"], // def/defmodule are calls
        "hcl" => &["block"],
        "toml" => &["table", "table_array_element"],
        "yaml" => &["block_mapping_pair"],
        "json" => &["pair"],
        "markdown" => &["section", "atx_heading"],
        "html" => &["element"],
        "css" => &["rule_set"],
        "proto" => &["message", "service", "rpc", "enum"],
        _ => &[],
    }
}

const MAX_NAME_CHARS: usize = 60;

fn first_line(source: &[u8], node: &Node) -> Option<String> {
    let s = node.utf8_text(source).ok()?;
    let line = s.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return None;
    }
    Some(line.chars().take(MAX_NAME_CHARS).collect())
}

/// Extract the human-readable name from a symbol node, where the grammar
/// provides a "name" field. Falls back to the first identifier child, then
/// to the first named child's text for nodes without a name.
fn node_name(source: &[u8], node: &Node) -> Option<String> {
    if node.kind() == "block"
        && let Some(name) = hcl_block_name(source, node)
    {
        return Some(name);
    }
    if let Some(name) = node
        .child_by_field_name("name")
        .and_then(|n| first_line(source, &n))
    {
        return Some(name);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if (child.kind() == "identifier" || child.kind() == "type_identifier")
            && let Some(name) = first_line(source, &child)
        {
            return Some(name);
        }
    }
    let mut cursor = node.walk();
    let mut named = node.named_children(&mut cursor);
    let first = named.next()?;
    match node.kind() {
        "section" => node_name(source, &first),
        "atx_heading" => named
            .find(|c| c.kind() == "inline")
            .and_then(|c| first_line(source, &c)),
        _ => first_line(source, &first),
    }
}

fn is_symbol(node: &Node) -> bool {
    node.kind() != "variable_declarator"
        || node.child_by_field_name("value").is_some_and(|v| {
            matches!(
                v.kind(),
                "arrow_function" | "function_expression" | "function"
            )
        })
}

fn hcl_block_name(source: &[u8], node: &Node) -> Option<String> {
    let mut cursor = node.walk();
    let parts: Vec<&str> = node
        .named_children(&mut cursor)
        .filter(|c| matches!(c.kind(), "identifier" | "string_lit"))
        .filter_map(|c| c.utf8_text(source).ok())
        .map(|t| t.trim_matches('"'))
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(parts.join(" ").chars().take(MAX_NAME_CHARS).collect())
}

fn parse(source: &str, lang: &str) -> Option<Tree> {
    let language = tslp::get_language(lang).ok()?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    parser.parse(source, None)
}

fn read_contained(workdir: &Path, rel: &str) -> Option<String> {
    let rel = Path::new(rel);
    if rel.is_absolute() || rel.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    let root = workdir.canonicalize().ok()?;
    let full = root.join(rel).canonicalize().ok()?;
    if !full.starts_with(&root) || !full.metadata().ok()?.is_file() {
        return None;
    }
    std::fs::read_to_string(full).ok()
}

/// Find the enclosing symbol for a hunk range in the new file.
///
/// Strategy: rather than "walk up from smallest node spanning the
/// range" (which collapses to root for multi-function hunks), we
/// descend the tree looking for the FIRST top-level symbol node that
/// intersects the range. If several symbols intersect (the hunk spans
/// `main` + `retry_loop` + helpers), report the structurally most
/// significant one — the one whose start falls earliest. This matches
/// the reviewer's intuition: 'this hunk changes main() and adds
/// retry_loop' gets labeled 'retry_loop' because retry_loop is new
/// content; main() is edited context.
pub fn find_enclosing_symbol(
    tree: &Tree,
    source: &str,
    lang: &str,
    start: u32,
    end: u32,
) -> Option<(String, String)> {
    let root = tree.root_node();

    let kinds = symbol_kinds(lang);
    let (start_row, end_row) = (
        start.saturating_sub(1) as usize,
        end.saturating_sub(1) as usize,
    );

    // BFS/traverse: collect all symbol-kind nodes intersecting the
    // range, pick the one starting earliest inside the range (added
    // code = the change's center of gravity).
    let mut best: Option<Node> = None;
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        let (ns, ne) = (n.start_position().row, n.end_position().row);
        if ne < start_row || ns > end_row {
            continue; // no overlap
        }
        if kinds.contains(&n.kind()) && is_symbol(&n) {
            // Prefer symbols that START inside the range (new content)
            // over those merely edited (started before).
            let starts_inside = ns >= start_row;
            let candidate_better = match &best {
                None => true,
                Some(b) => {
                    let bs = b.start_position().row;
                    let b_inside = bs >= start_row;
                    (starts_inside && !b_inside) || (starts_inside == b_inside && ns < bs)
                }
            };
            if candidate_better {
                best = Some(n);
            }
        }
        // descend
        let mut cur = n.walk();
        for c in n.children(&mut cur) {
            stack.push(c);
        }
    }
    let node = best?;
    let name = node_name(source.as_bytes(), &node)?;
    Some((node.kind().to_string(), name))
}

/// Extract hunk-header context (the text after the second `@@`).
fn header_context(raw: &RawHunk) -> String {
    let rest = raw.header.strip_prefix("@@ ").unwrap_or("");
    if let Some(idx) = rest.find(" @@") {
        rest[idx + 3..].trim().to_string()
    } else {
        String::new()
    }
}

/// Enrich all hunks in all files. Reads new-side file contents from the
/// working tree at the given root.
pub fn enrich(files: &[FileDiff], workdir: &Path) -> EnrichedDiff {
    let mut out_files = Vec::new();
    for f in files {
        let new_path = if f.new_path == "/dev/null" {
            &f.old_path
        } else {
            &f.new_path
        };
        let lang = lang_for(new_path).unwrap_or("text").to_string();
        let parsed = if lang != "text" {
            read_contained(workdir, new_path).and_then(|src| parse(&src, &lang).map(|t| (src, t)))
        } else {
            None
        };

        let mut hunks = Vec::new();
        for h in &f.hunks {
            let (sym_kind, sym_name) = match &parsed {
                Some((src, tree)) => {
                    find_enclosing_symbol(tree, src, &lang, h.new_start, h.new_end)
                        .map(|(k, n)| (Some(k), Some(n)))
                        .unwrap_or((None, None))
                }
                None => (None, None),
            };
            hunks.push(EnrichedHunk {
                file: new_path.clone(),
                new_start: h.new_start,
                new_end: h.new_end,
                old_start: h.old_start,
                old_end: h.old_end,
                language: lang.clone(),
                enclosing_symbol: sym_name,
                enclosing_kind: sym_kind,
                hunk_header_context: header_context(h),
                body_preview: h.body.lines().take(30).collect::<Vec<_>>().join("\n"),
            });
        }

        out_files.push(EnrichedFile {
            old_path: f.old_path.clone(),
            new_path: f.new_path.clone(),
            renamed: f.renamed,
            language: lang,
            hunks,
        });
    }
    EnrichedDiff { files: out_files }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(lang: &str, src: &str, line: u32) -> Option<(String, String)> {
        let tree = parse(src, lang)?;
        find_enclosing_symbol(&tree, src, lang, line, line)
    }

    fn assert_symbol(lang: &str, src: &str, line: u32, kind: &str, name: &str) {
        assert_eq!(
            symbol(lang, src, line),
            Some((kind.to_string(), name.to_string())),
            "{lang}"
        );
    }

    #[test]
    fn rust_function() {
        assert_symbol(
            "rust",
            "fn a() {}\nfn main() {\n    let x = 1;\n}\n",
            3,
            "function_item",
            "main",
        );
    }

    #[test]
    fn javascript_function() {
        assert_symbol(
            "javascript",
            "function hello() {\n  return 1;\n}\n",
            2,
            "function_declaration",
            "hello",
        );
    }

    #[test]
    fn javascript_arrow_declarator() {
        assert_symbol(
            "javascript",
            "const add = (a, b) => {\n  return a + b;\n};\n",
            2,
            "variable_declarator",
            "add",
        );
    }

    #[test]
    fn plain_const_inside_function_resolves_to_the_function() {
        let src = "function foo() {\n  const x = 1;\n  return x;\n}\n";
        assert_symbol("javascript", src, 2, "function_declaration", "foo");
        assert_symbol("typescript", src, 2, "function_declaration", "foo");
    }

    #[test]
    fn typescript_interface() {
        assert_symbol(
            "typescript",
            "interface Shape {\n  area: number;\n}\n",
            2,
            "interface_declaration",
            "Shape",
        );
    }

    #[test]
    fn typescript_type_alias_and_enum() {
        assert_symbol(
            "typescript",
            "type Id = string;\nenum Color {\n  Red,\n}\n",
            1,
            "type_alias_declaration",
            "Id",
        );
        assert_symbol(
            "typescript",
            "type Id = string;\nenum Color {\n  Red,\n}\n",
            3,
            "enum_declaration",
            "Color",
        );
    }

    #[test]
    fn hcl_block() {
        assert_symbol(
            "hcl",
            "resource \"aws_s3_bucket\" \"b\" {\n  bucket = \"x\"\n}\n",
            2,
            "block",
            "resource aws_s3_bucket b",
        );
    }

    #[test]
    fn toml_table() {
        assert_symbol(
            "toml",
            "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\n",
            5,
            "table",
            "dependencies",
        );
    }

    #[test]
    fn toml_array_table() {
        assert_symbol(
            "toml",
            "[[bin]]\nname = \"a\"\n",
            2,
            "table_array_element",
            "bin",
        );
    }

    #[test]
    fn yaml_mapping_pair() {
        assert_symbol(
            "yaml",
            "services:\n  web:\n    image: nginx\n",
            3,
            "block_mapping_pair",
            "image",
        );
    }

    #[test]
    fn json_pair() {
        assert_symbol("json", "{\n  \"name\": \"x\"\n}\n", 2, "pair", "\"name\"");
    }

    #[test]
    fn markdown_section() {
        let (_, name) = symbol("markdown", "# Title\n\nintro\n\n## Usage\n\nrun it\n", 5).unwrap();
        assert_eq!(name, "Usage");
    }

    #[test]
    fn html_element() {
        let (kind, name) = symbol("html", "<div class=\"a\">\n  <p>hi</p>\n</div>\n", 2).unwrap();
        assert_eq!(kind, "element");
        assert!(name.starts_with("<p"), "{name}");
    }

    #[test]
    fn css_rule_set() {
        assert_symbol("css", ".a {\n  color: red;\n}\n", 2, "rule_set", ".a");
    }

    #[test]
    fn proto_message_and_rpc() {
        let src = "syntax = \"proto3\";\nmessage Req {\n  string id = 1;\n}\nservice Svc {\n  rpc Get(Req) returns (Req);\n}\n";
        assert_symbol("proto", src, 3, "message", "Req");
        assert_symbol("proto", src, 6, "rpc", "Get");
    }

    #[test]
    fn long_names_are_truncated() {
        let key = "k".repeat(100);
        let src = format!("{{\n  \"{key}\": 1\n}}\n");
        let (_, name) = symbol("json", &src, 2).unwrap();
        assert_eq!(name.chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn read_contained_rejects_escaping_paths() {
        let dir = std::env::temp_dir().join(format!("adhdiff-enrich-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ok.rs"), "fn a() {}\n").unwrap();
        assert!(read_contained(&dir, "ok.rs").is_some());
        assert!(read_contained(&dir, "../x.rs").is_none());
        assert!(read_contained(&dir, "a/../../x.rs").is_none());
        assert!(read_contained(&dir, "/etc/passwd").is_none());
        assert!(read_contained(&dir, ".").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
