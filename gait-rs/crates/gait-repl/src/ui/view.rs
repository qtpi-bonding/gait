//! View-model layer for the REPL. Transforms ReplItem (the data model) into
//! ReplViewItem (the rendering layer) and owns the pure display logic — no
//! terminal I/O lives here.

/// Render-ready item. The renderer matches on these variants; column conventions
/// are encoded by the variant, not by literal strings at the call site.
pub enum ReplViewItem {
    /// A numbered assistant text block — full markdown content rendered through termimad.
    Block {
        num: usize,
        content: String,
    },
    /// First line of a structured_done message — rendered with ✓│ gutter.
    DoneStart {
        text: String,
    },
    /// Continuation lines for DoneStart (multi-line done messages).
    DoneCont {
        text: String,
    },
    ToolResult {
        ok: bool,
        name: String,
        elapsed_ms: Option<u64>,
    },
    SnippetLine {
        text: String,
    },
    Error {
        message: String,
    },
    TurnSummary {
        elapsed_ms: u64,
        output_tokens: u64,
    },
    StructuredBox {
        json: String,
    },
    Spacing,
}

/// Transform one ReplItem into the view items the renderer prints.
/// Pure — no terminal I/O.
pub(crate) fn clean_repl_item(item: crate::ui::state::ReplItem) -> Vec<ReplViewItem> {
    use crate::ui::state::ReplItem;
    match item {
        ReplItem::Block { content, block_num } => {
            if content.is_empty() {
                vec![]
            } else {
                vec![ReplViewItem::Block {
                    num: block_num,
                    content,
                }]
            }
        }
        ReplItem::Spacing => vec![ReplViewItem::Spacing],
        ReplItem::ToolResult {
            ok,
            name,
            elapsed_ms,
            output,
        } => {
            let mut out = vec![ReplViewItem::ToolResult {
                ok,
                name,
                elapsed_ms,
            }];
            for line in clean_tool_output(&output) {
                out.push(ReplViewItem::SnippetLine { text: line });
            }
            out
        }
        ReplItem::Error { message } => vec![ReplViewItem::Error { message }],
        ReplItem::TurnSummary {
            elapsed_ms,
            output_tokens,
        } => vec![ReplViewItem::TurnSummary {
            elapsed_ms,
            output_tokens,
        }],
        ReplItem::DoneMessage { content } => {
            let mut lines = content.lines();
            if let Some(first) = lines.next() {
                let mut out = vec![ReplViewItem::DoneStart {
                    text: first.to_string(),
                }];
                for line in lines {
                    out.push(ReplViewItem::DoneCont {
                        text: line.to_string(),
                    });
                }
                out
            } else {
                vec![]
            }
        }
        ReplItem::StructuredResult { json } => vec![ReplViewItem::StructuredBox { json }],
    }
}

/// Strip hashline anchors, XML wrappers, and footer lines from raw tool output.
/// Returns at most 3 lines: first, `…`, last (so the snippet stays compact).
pub(crate) fn clean_tool_output(raw: &str) -> Vec<String> {
    let lines: Vec<String> = raw
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty()
                && t != "<file>"
                && t != "</file>"
                && !t.starts_with("(End of file")
                && !t.starts_with("(File has more")
                && !t.starts_with("(Output truncated")
        })
        .map(|l| match strip_hashline(l) {
            Some(content) => content.to_string(),
            None => l.to_string(),
        })
        .collect();
    match lines.len() {
        0 | 1 | 2 => lines,
        _ => vec![
            lines[0].clone(),
            "\u{2026}".to_string(), // …
            lines[lines.len() - 1].clone(),
        ],
    }
}

/// Strip `#XY|` or `#XY-N|` prefix from a hashline. Returns the content after `|`,
/// or `None` for non-hashlines. `XY` must be exactly 2 uppercase ASCII chars;
/// `-N` (if present) must be a non-empty digit suffix.
fn strip_hashline(s: &str) -> Option<&str> {
    let rest = s.strip_prefix('#')?;
    let (tag, content) = rest.split_once('|')?;
    if tag.len() < 2 {
        return None;
    }
    let (hash, suffix) = tag.split_at(2);
    if !hash.chars().all(|c| c.is_ascii_uppercase()) {
        return None;
    }
    if !suffix.is_empty() {
        let n = suffix.strip_prefix('-')?;
        if n.is_empty() || !n.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
    }
    Some(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::state::ReplItem;

    #[test]
    fn clean_tool_output_strips_hashline_anchors() {
        // Unique line: #XY|content
        assert_eq!(clean_tool_output("#CD|fn foo() {"), vec!["fn foo() {"]);
        // Duplicate instance: #XY-N|content
        assert_eq!(clean_tool_output("#AB-2|fn foo() {"), vec!["fn foo() {"]);
    }

    #[test]
    fn clean_tool_output_drops_xml_and_footers() {
        let raw = "<file>\n#AB|pub fn foo() {\n#CD|}\n(End of file - total 2 lines)\n</file>";
        assert_eq!(clean_tool_output(raw), vec!["pub fn foo() {", "}"]);
    }

    #[test]
    fn clean_repl_item_block_emits_single_block_item() {
        let item = ReplItem::Block {
            content: "first line\nsecond line\nthird".to_string(),
            block_num: 7,
        };
        let out = clean_repl_item(item);
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0], ReplViewItem::Block { num: 7, content } if content == "first line\nsecond line\nthird")
        );
    }

    #[test]
    fn clean_repl_item_empty_block_is_suppressed() {
        let item = ReplItem::Block {
            content: String::new(),
            block_num: 3,
        };
        let out = clean_repl_item(item);
        assert!(out.is_empty(), "empty block produces no view items");
    }

    #[test]
    fn clean_repl_item_tool_result_emits_header_and_snippets() {
        let item = ReplItem::ToolResult {
            ok: true,
            name: "read".to_string(),
            elapsed_ms: Some(42),
            // Two identical hashlines → 2 unique content lines (≤2, no truncation).
            output: "<file>\n#AB|hello\n#AB-2|world\n</file>".to_string(),
        };
        let out = clean_repl_item(item);
        assert!(
            matches!(out[0], ReplViewItem::ToolResult { ok: true, ref name, elapsed_ms: Some(42) } if name == "read")
        );
        assert!(matches!(out[1], ReplViewItem::SnippetLine { ref text } if text == "hello"));
        assert!(matches!(out[2], ReplViewItem::SnippetLine { ref text } if text == "world"));
    }

    #[test]
    fn clean_tool_output_truncates_long_output() {
        let raw = "line1\nline2\nline3\nline4\nline5";
        let out = clean_tool_output(raw);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], "line1");
        assert_eq!(out[1], "\u{2026}");
        assert_eq!(out[2], "line5");
    }

    #[test]
    fn clean_repl_item_error_maps_directly() {
        let item = ReplItem::Error {
            message: "boom".into(),
        };
        let out = clean_repl_item(item);
        assert!(matches!(out[0], ReplViewItem::Error { ref message } if message == "boom"));
    }

    #[test]
    fn clean_repl_item_turn_summary_maps_directly() {
        let item = ReplItem::TurnSummary {
            elapsed_ms: 1500,
            output_tokens: 250,
        };
        let out = clean_repl_item(item);
        assert!(matches!(
            out[0],
            ReplViewItem::TurnSummary {
                elapsed_ms: 1500,
                output_tokens: 250
            }
        ));
    }

    #[test]
    fn clean_repl_item_structured_result_maps_directly() {
        let item = ReplItem::StructuredResult { json: "{}".into() };
        let out = clean_repl_item(item);
        assert!(matches!(out[0], ReplViewItem::StructuredBox { ref json } if json == "{}"));
    }

    #[test]
    fn clean_repl_item_spacing_maps_directly() {
        let out = clean_repl_item(ReplItem::Spacing);
        assert!(matches!(out[0], ReplViewItem::Spacing));
    }
}
