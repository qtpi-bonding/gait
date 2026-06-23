use std::collections::HashSet;

/// Returns numeric @(N) indices found in `line`, in order, deduplicated.
/// Only complete tokens (with closing `)`) are returned.
pub fn collect_at_refs(line: &str) -> Vec<usize> {
    let chars: Vec<char> = line.chars().collect();
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' && i + 1 < chars.len() && chars[i + 1] == '(' {
            let inner_start = i + 2;
            if let Some(rel) = chars[inner_start..].iter().position(|&c| c == ')') {
                let inner: String = chars[inner_start..inner_start + rel].iter().collect();
                if let Ok(n) = inner.trim().parse::<usize>() {
                    if n >= 1 && !seen.contains(&n) {
                        result.push(n);
                        seen.insert(n);
                    }
                }
                i = inner_start + rel + 1;
                continue;
            }
        }
        i += 1;
    }
    result
}

// Keep deprecated stubs so chat.rs/driver.rs can still compile until
// those files are migrated in this task (they will be updated below).
// TODO: remove after Steps 7-9 are done.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_refs_returns_empty() {
        assert_eq!(collect_at_refs("explain this error"), Vec::<usize>::new());
    }

    #[test]
    fn single_ref_returned() {
        assert_eq!(collect_at_refs("explain @(1)"), vec![1]);
    }

    #[test]
    fn multiple_refs_in_order() {
        assert_eq!(collect_at_refs("diff @(1) and @(2)"), vec![1, 2]);
    }

    #[test]
    fn duplicate_ref_deduplicated() {
        assert_eq!(collect_at_refs("@(1) vs @(1)"), vec![1]);
    }

    #[test]
    fn partial_token_ignored() {
        assert_eq!(collect_at_refs("@(2 is incomplete"), Vec::<usize>::new());
    }

    #[test]
    fn non_numeric_token_ignored() {
        assert_eq!(collect_at_refs("@(echo hi)"), Vec::<usize>::new());
    }

    #[test]
    fn zero_ref_ignored() {
        assert_eq!(collect_at_refs("@(0) is invalid"), Vec::<usize>::new());
    }

    #[test]
    fn two_digit_ref() {
        assert_eq!(collect_at_refs("@(12)"), vec![12]);
    }
}
