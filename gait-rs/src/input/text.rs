//! Plain-text helpers shared across layers (no UI, no I/O).

/// Split markdown text into numbered blocks using paragraph/fence logic.
/// An empty line terminates a block; a fenced code section is treated as one block.
pub fn split_into_blocks(text: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_fence = false;
    for line in text.lines() {
        let is_fence = line.trim_start().starts_with("```");
        if in_fence {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(line);
            if is_fence {
                in_fence = false;
                let block = std::mem::take(&mut current);
                if !block.is_empty() {
                    blocks.push(block);
                }
            }
        } else if line.is_empty() {
            let block = std::mem::take(&mut current);
            if !block.is_empty() {
                blocks.push(block);
            }
        } else {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(line);
            if is_fence {
                in_fence = true;
            }
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_into_blocks_paragraphs() {
        let text = "para one\n\npara two\n\npara three";
        let blocks = split_into_blocks(text);
        assert_eq!(blocks, vec!["para one", "para two", "para three"]);
    }

    #[test]
    fn split_into_blocks_fenced_code() {
        let text = "intro\n\n```rust\nlet x = 1;\n```\n\noutro";
        let blocks = split_into_blocks(text);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0], "intro");
        assert!(blocks[1].contains("let x = 1;"));
        assert_eq!(blocks[2], "outro");
    }
}
