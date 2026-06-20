//! Pure rendering of a tool's pending change for the permission prompt.
//! The tool layer owns this content; the UI renders the returned string blind to format.

use similar::{ChangeTag, TextDiff};

/// Unified `-`/`+` diff of `old`→`new` with 2 lines of context.
/// Returns `None` when the inputs are identical (no change to preview).
/// No truncation — the full diff is returned. No `@@` hunk headers.
pub fn diff_preview(old: &str, new: &str) -> Option<String> {
    if old == new {
        return None;
    }
    let diff = TextDiff::from_lines(old, new);
    let mut out = String::new();
    for group in diff.grouped_ops(2) {
        for op in group.iter() {
            for change in diff.iter_changes(op) {
                let sign = match change.tag() {
                    ChangeTag::Delete => '-',
                    ChangeTag::Insert => '+',
                    ChangeTag::Equal => ' ',
                };
                out.push(sign);
                out.push_str(change.value()); // value() includes the line's trailing '\n'
            }
        }
    }
    Some(out.trim_end_matches('\n').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_line_change_shows_minus_plus_and_context() {
        let old = "alpha\nbravo\ncharlie\n";
        let new = "alpha\nBRAVO\ncharlie\n";
        let out = diff_preview(old, new).expect("changed input → Some");
        assert!(out.contains("-bravo"), "missing deleted line:\n{out}");
        assert!(out.contains("+BRAVO"), "missing inserted line:\n{out}");
        assert!(out.contains(" alpha"), "missing context line:\n{out}");
        assert!(!out.contains("@@"), "should not contain @@ headers:\n{out}");
    }

    #[test]
    fn identical_input_is_none() {
        assert_eq!(diff_preview("same\ntext\n", "same\ntext\n"), None);
    }

    #[test]
    fn multi_hunk_change_shows_both_hunks() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let new = "A\nb\nc\nd\ne\nf\ng\nh\ni\nJ\n";
        let out = diff_preview(old, new).expect("changed input → Some");
        assert!(
            out.contains("-a") && out.contains("+A"),
            "first hunk missing:\n{out}"
        );
        assert!(
            out.contains("-j") && out.contains("+J"),
            "second hunk missing:\n{out}"
        );
    }

    #[test]
    fn handles_no_trailing_newline() {
        let out = diff_preview("old", "new").expect("changed input → Some");
        assert!(out.contains("-old"), "preview:\n{out}");
        assert!(out.contains("+new"), "preview:\n{out}");
    }
}
