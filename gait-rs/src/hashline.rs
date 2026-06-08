/// Hashline: content-hash-anchored line references for reliable file edits.
///
/// Read output format:  `42#NB|fn render_help() {`
/// Edit ref format:     `42#NB`
///
/// Hash is xxHash32(trimmed_content, seed) % 256 mapped to a 2-char code.
/// Seed is 0 for lines with alphanumeric content (position-independent),
/// or the line number for structural lines like `{`, `}`, blank lines
/// (gives distinct hashes to identical structural chars at different positions).
///
/// Ported from oh-my-openagent (MIT) — github.com/code-yeongyu/oh-my-openagent

// ── xxHash32 ─────────────────────────────────────────────────────────────────

const PRIME32_1: u32 = 0x9e3779b1;
const PRIME32_2: u32 = 0x85ebca77;
const PRIME32_3: u32 = 0xc2b2ae3d;
const PRIME32_4: u32 = 0x27d4eb2f;
const PRIME32_5: u32 = 0x165667b1;

fn rotate_left_32(v: u32, bits: u32) -> u32 {
    v.rotate_left(bits)
}

fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    let b = |i: usize| data.get(offset + i).copied().unwrap_or(0) as u32;
    b(0) | (b(1) << 8) | (b(2) << 16) | (b(3) << 24)
}

fn round32(acc: u32, val: u32) -> u32 {
    rotate_left_32(acc.wrapping_add(val.wrapping_mul(PRIME32_2)), 13).wrapping_mul(PRIME32_1)
}

pub fn xxhash32(input: &[u8], seed: u32) -> u32 {
    let length = input.len();
    let mut offset = 0usize;
    let mut hash: u32;

    if length >= 16 {
        let limit = length - 16;
        let mut v1 = seed.wrapping_add(PRIME32_1).wrapping_add(PRIME32_2);
        let mut v2 = seed.wrapping_add(PRIME32_2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(PRIME32_1);

        while offset <= limit {
            v1 = round32(v1, read_u32_le(input, offset));     offset += 4;
            v2 = round32(v2, read_u32_le(input, offset));     offset += 4;
            v3 = round32(v3, read_u32_le(input, offset));     offset += 4;
            v4 = round32(v4, read_u32_le(input, offset));     offset += 4;
        }

        hash = rotate_left_32(v1, 1)
            .wrapping_add(rotate_left_32(v2, 7))
            .wrapping_add(rotate_left_32(v3, 12))
            .wrapping_add(rotate_left_32(v4, 18));
    } else {
        hash = seed.wrapping_add(PRIME32_5);
    }

    hash = hash.wrapping_add(length as u32);

    while offset + 4 <= length {
        hash = hash.wrapping_add(read_u32_le(input, offset).wrapping_mul(PRIME32_3));
        hash = rotate_left_32(hash, 17).wrapping_mul(PRIME32_4);
        offset += 4;
    }

    while offset < length {
        hash = hash.wrapping_add((input[offset] as u32).wrapping_mul(PRIME32_5));
        hash = rotate_left_32(hash, 11).wrapping_mul(PRIME32_1);
        offset += 1;
    }

    hash ^= hash >> 15;
    hash = hash.wrapping_mul(PRIME32_2);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(PRIME32_3);
    hash ^= hash >> 16;
    hash
}

// ── Dictionary ────────────────────────────────────────────────────────────────

const NIBBLE: &[u8] = b"ZPMQVRWSNKTXJBYH";

/// Map a 0-255 index to a two-char hash code using the nibble alphabet.
fn dict_entry(i: usize) -> [u8; 2] {
    [NIBBLE[i >> 4], NIBBLE[i & 0xf]]
}

fn entry_str(i: usize) -> String {
    let b = dict_entry(i);
    String::from_utf8(b.to_vec()).unwrap()
}

// ── Hash computation ──────────────────────────────────────────────────────────

fn has_significant(s: &str) -> bool {
    s.chars().any(|c| c.is_alphanumeric())
}

pub fn compute_line_hash(line_number: usize, content: &str) -> String {
    // Strip \r and trailing whitespace (normalised content)
    let normalized: String = content
        .trim_end_matches('\r')
        .trim_end()
        .to_string();
    let seed = if has_significant(&normalized) {
        0u32
    } else {
        line_number as u32
    };
    let h = xxhash32(normalized.as_bytes(), seed);
    entry_str((h % 256) as usize)
}

/// Format one line as `N#XY|content` (1-indexed).
pub fn format_hash_line(line_number: usize, content: &str) -> String {
    let hash = compute_line_hash(line_number, content);
    format!("{}#{}|{}", line_number, hash, content)
}

/// Annotate every line of `content` with hashline prefixes.
pub fn format_hash_lines(content: &str) -> String {
    if content.is_empty() {
        return String::new();
    }
    content
        .split('\n')
        .enumerate()
        .map(|(i, line)| format_hash_line(i + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Parsing & validation ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LineRef {
    pub line: usize,
    pub hash: String,
}

fn is_valid_hash(s: &str) -> bool {
    s.len() == 2 && s.bytes().all(|b| NIBBLE.contains(&b))
}

/// Parse `N#XY` into a `LineRef`. Tolerates extra whitespace and strips `N#XY|…` tails.
pub fn parse_line_ref(ref_str: &str) -> anyhow::Result<LineRef> {
    let s = ref_str.trim();
    // Strip full hashline output format `N#XY|content` → `N#XY`
    let s = s.split('|').next().unwrap_or(s).trim();
    let Some(hash_pos) = s.find('#') else {
        anyhow::bail!("Invalid line reference \"{ref_str}\": expected format N#XY");
    };
    let line_part = &s[..hash_pos];
    let hash_part = &s[hash_pos + 1..];
    let Ok(line) = line_part.parse::<usize>() else {
        anyhow::bail!("Invalid line reference \"{ref_str}\": \"{}\" is not a line number", line_part);
    };
    if line == 0 {
        anyhow::bail!("Invalid line reference \"{ref_str}\": line numbers are 1-indexed");
    }
    if !is_valid_hash(hash_part) {
        anyhow::bail!("Invalid line reference \"{ref_str}\": \"{hash_part}\" is not a valid 2-char hash");
    }
    Ok(LineRef { line, hash: hash_part.to_string() })
}

fn is_compatible_hash(line_number: usize, content: &str, expected: &str) -> bool {
    compute_line_hash(line_number, content) == expected
}

const MISMATCH_CONTEXT: usize = 2;

#[derive(Debug)]
pub struct HashlineMismatchError {
    pub message: String,
}

impl std::fmt::Display for HashlineMismatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for HashlineMismatchError {}

fn mismatch_message(mismatches: &[(usize, String)], file_lines: &[&str]) -> String {
    let mismatch_set: std::collections::HashSet<usize> =
        mismatches.iter().map(|(l, _)| *l).collect();

    let mut display_lines = std::collections::BTreeSet::new();
    for (line, _) in mismatches {
        let lo = line.saturating_sub(MISMATCH_CONTEXT).max(1);
        let hi = (line + MISMATCH_CONTEXT).min(file_lines.len());
        for l in lo..=hi {
            display_lines.insert(l);
        }
    }

    let count = mismatches.len();
    let mut out = format!(
        "{} line{} changed since last read. Use updated {{line_number}}#{{hash_id}} references below (>>> marks changed lines).\n",
        count,
        if count == 1 { " has" } else { "s have" }
    );

    let mut prev = 0usize;
    for line in &display_lines {
        if prev > 0 && *line > prev + 1 {
            out.push_str("    ...\n");
        }
        prev = *line;
        let content = file_lines.get(line - 1).copied().unwrap_or("");
        let hash = compute_line_hash(*line, content);
        let prefix = format!("{}#{}|{}", line, hash, content);
        if mismatch_set.contains(line) {
            out.push_str(&format!(">>> {}\n", prefix));
        } else {
            out.push_str(&format!("    {}\n", prefix));
        }
    }

    // Suggest where a mismatched hash might have moved
    for (_, expected_hash) in mismatches {
        for (i, &content) in file_lines.iter().enumerate() {
            if is_compatible_hash(i + 1, content, expected_hash) {
                let actual = compute_line_hash(i + 1, content);
                out.push_str(&format!("\nHint: content with hash #{} is now at line {}#{}\n", expected_hash, i + 1, actual));
                break;
            }
        }
    }

    out.trim_end().to_string()
}

/// Validate that all `refs` still match their lines in `file_lines`.
pub fn validate_line_refs(file_lines: &[&str], refs: &[String]) -> anyhow::Result<()> {
    let mut mismatches: Vec<(usize, String)> = Vec::new();
    for ref_str in refs {
        let lr = parse_line_ref(ref_str)?;
        if lr.line < 1 || lr.line > file_lines.len() {
            anyhow::bail!(
                "Line {} out of bounds (file has {} lines)",
                lr.line,
                file_lines.len()
            );
        }
        let content = file_lines[lr.line - 1];
        if !is_compatible_hash(lr.line, content, &lr.hash) {
            mismatches.push((lr.line, lr.hash.clone()));
        }
    }
    if !mismatches.is_empty() {
        return Err(HashlineMismatchError {
            message: mismatch_message(&mismatches, file_lines),
        }
        .into());
    }
    Ok(())
}

// ── Edit types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct HashlineEditOp {
    /// Operation: "replace", "append" (insert after), or "prepend" (insert before)
    pub op: String,
    /// Line reference (e.g. "42#NB") for the target line.
    /// Required for replace and positional append/prepend. Omit for append/prepend to file end/start.
    pub pos: Option<String>,
    /// End line reference for a replace range (e.g. "44#XQ"). Omit for single-line replace.
    pub end: Option<String>,
    /// Replacement or insertion lines (do NOT include the old lines — only the new content)
    pub lines: Vec<String>,
}

// ── Deduplication & ordering ─────────────────────────────────────────────────

fn dedupe_ops(ops: &[HashlineEditOp]) -> (Vec<HashlineEditOp>, usize) {
    let mut seen = std::collections::HashSet::new();
    let mut deduped = Vec::new();
    let mut dropped = 0usize;
    for op in ops {
        let key = format!("{}|{}|{}|{}", op.op,
            op.pos.as_deref().unwrap_or(""),
            op.end.as_deref().unwrap_or(""),
            op.lines.join("\x00"));
        if seen.insert(key) {
            deduped.push(op.clone());
        } else {
            dropped += 1;
        }
    }
    (deduped, dropped)
}

fn op_sort_line(op: &HashlineEditOp) -> i64 {
    // Sort by the highest referenced line number descending so bottom edits go first.
    let ref_for_sort = op.end.as_deref()
        .or(op.pos.as_deref());
    match ref_for_sort.and_then(|r| parse_line_ref(r).ok()) {
        Some(lr) => lr.line as i64,
        None => i64::MIN,
    }
}

fn detect_overlapping_ranges(ops: &[HashlineEditOp]) -> Option<String> {
    let mut ranges: Vec<(usize, usize, usize)> = Vec::new(); // (start, end, idx)
    for (i, op) in ops.iter().enumerate() {
        if op.op != "replace" { continue; }
        let Some(ref pos) = op.pos else { continue; };
        let start = match parse_line_ref(pos) { Ok(r) => r.line, Err(_) => continue };
        let end = match op.end.as_deref().map(parse_line_ref) {
            Some(Ok(r)) => r.line,
            _ => start,
        };
        ranges.push((start, end, i));
    }
    ranges.sort_by_key(|&(s, e, _)| (s, e));
    for w in ranges.windows(2) {
        let (s1, e1, i1) = w[0];
        let (s2, _, i2) = w[1];
        if s2 <= e1 {
            return Some(format!(
                "Overlapping replace ranges: edit {} (lines {}-{}) overlaps edit {} (starts at line {})",
                i1 + 1, s1, e1, i2 + 1, s2
            ));
        }
    }
    None
}

// ── Apply ─────────────────────────────────────────────────────────────────────

/// Apply a list of `HashlineEditOp` to `content`. Validates all refs upfront before
/// writing, deduplicates identical ops, and applies bottom-up to preserve line numbers.
pub fn apply_hashline_edits(content: &str, ops: &[HashlineEditOp]) -> anyhow::Result<String> {
    if ops.is_empty() {
        return Ok(content.to_string());
    }

    let (ops, dupes) = dedupe_ops(ops);
    if dupes > 0 {
        eprintln!("[hashline] dropped {} duplicate edit(s)", dupes);
    }

    let mut lines: Vec<&str> = if content.is_empty() { vec![] } else { content.split('\n').collect() };

    // Collect and validate all refs upfront (fail before any write)
    let refs: Vec<String> = ops
        .iter()
        .flat_map(|op| {
            let mut v = Vec::new();
            if let Some(p) = &op.pos { v.push(p.clone()); }
            if let Some(e) = &op.end { v.push(e.clone()); }
            v
        })
        .collect();
    validate_line_refs(&lines, &refs)?;

    if let Some(msg) = detect_overlapping_ranges(&ops) {
        anyhow::bail!("{}", msg);
    }

    // Sort descending by line so bottom edits go first → earlier line numbers stay valid
    let mut sorted = ops.clone();
    sorted.sort_by(|a, b| op_sort_line(b).cmp(&op_sort_line(a)));

    let mut owned: Vec<String> = lines.iter().map(|s| s.to_string()).collect();

    for op in &sorted {
        match op.op.as_str() {
            "replace" => {
                let start = parse_line_ref(op.pos.as_deref().unwrap_or(""))?.line;
                let end = match op.end.as_deref().map(parse_line_ref) {
                    Some(Ok(r)) => r.line,
                    _ => start,
                };
                if start > end {
                    anyhow::bail!("replace: start line {} > end line {}", start, end);
                }
                owned.splice((start - 1)..end, op.lines.iter().cloned());
            }
            "append" => {
                match op.pos.as_deref().map(parse_line_ref) {
                    Some(Ok(lr)) => {
                        let idx = lr.line; // insert after line lr.line (0-indexed = lr.line)
                        for (i, line) in op.lines.iter().enumerate() {
                            owned.insert(idx + i, line.clone());
                        }
                    }
                    None => owned.extend(op.lines.iter().cloned()),
                    Some(Err(e)) => return Err(e),
                }
            }
            "prepend" => {
                match op.pos.as_deref().map(parse_line_ref) {
                    Some(Ok(lr)) => {
                        let idx = lr.line - 1; // insert before line lr.line
                        for (i, line) in op.lines.iter().enumerate() {
                            owned.insert(idx + i, line.clone());
                        }
                    }
                    None => {
                        let mut new_lines = op.lines.clone();
                        new_lines.extend(owned);
                        owned = new_lines;
                    }
                    Some(Err(e)) => return Err(e),
                }
            }
            other => anyhow::bail!("Unknown op: \"{}\" — expected replace, append, or prepend", other),
        }
        // Refresh the borrowed slice after mutation
        lines = owned.iter().map(|s| s.as_str()).collect();
    }

    Ok(owned.join("\n"))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xxhash32_known_value() {
        // Empty input, seed 0 → known xxHash32 result
        let h = xxhash32(b"", 0);
        assert_eq!(h, 0x02cc5d05);
    }

    #[test]
    fn significant_line_is_position_independent() {
        let h1 = compute_line_hash(1, "fn render_help() {");
        let h2 = compute_line_hash(99, "fn render_help() {");
        assert_eq!(h1, h2, "significant lines should hash the same regardless of position");
    }

    #[test]
    fn structural_line_differs_by_position() {
        let h1 = compute_line_hash(5, "}");
        let h2 = compute_line_hash(50, "}");
        assert_ne!(h1, h2, "structural lines should differ by position");
    }

    #[test]
    fn format_hash_line_contains_parts() {
        let s = format_hash_line(42, "fn foo() {");
        assert!(s.starts_with("42#"), "got: {s}");
        assert!(s.contains("|fn foo() {"), "got: {s}");
        assert_eq!(s.matches('#').count(), 1);
    }

    #[test]
    fn parse_line_ref_basic() {
        let lr = parse_line_ref("42#NB").unwrap();
        assert_eq!(lr.line, 42);
        assert_eq!(lr.hash, "NB");
    }

    #[test]
    fn parse_line_ref_strips_content() {
        let lr = parse_line_ref("42#NB|fn foo() {").unwrap();
        assert_eq!(lr.line, 42);
        assert_eq!(lr.hash, "NB");
    }

    #[test]
    fn parse_line_ref_rejects_bad_hash() {
        assert!(parse_line_ref("42#12").is_err());
        assert!(parse_line_ref("42#ABC").is_err());
        assert!(parse_line_ref("0#NB").is_err());
    }

    #[test]
    fn validate_passes_for_correct_ref() {
        let content = "fn foo() {\n    bar();\n}";
        let lines: Vec<&str> = content.split('\n').collect();
        let hash = compute_line_hash(1, lines[0]);
        let ref_str = format!("1#{}", hash);
        validate_line_refs(&lines, &[ref_str]).unwrap();
    }

    #[test]
    fn validate_fails_and_shows_context() {
        let content = "fn foo() {\n    bar();\n}";
        let lines: Vec<&str> = content.split('\n').collect();
        let err = validate_line_refs(&lines, &["1#ZZ".to_string()]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(">>>"), "should mark changed line: {msg}");
        assert!(msg.contains("fn foo()"), "should show content: {msg}");
    }

    #[test]
    fn apply_replace_single_line() {
        let content = "line1\nline2\nline3";
        let lines: Vec<&str> = content.split('\n').collect();
        let hash = compute_line_hash(2, lines[1]);
        let result = apply_hashline_edits(
            content,
            &[HashlineEditOp {
                op: "replace".into(),
                pos: Some(format!("2#{}", hash)),
                end: None,
                lines: vec!["replaced".into()],
            }],
        )
        .unwrap();
        assert_eq!(result, "line1\nreplaced\nline3");
    }

    #[test]
    fn apply_replace_range() {
        let content = "a\nb\nc\nd";
        let file_lines: Vec<&str> = content.split('\n').collect();
        let h2 = compute_line_hash(2, file_lines[1]);
        let h3 = compute_line_hash(3, file_lines[2]);
        let result = apply_hashline_edits(
            content,
            &[HashlineEditOp {
                op: "replace".into(),
                pos: Some(format!("2#{}", h2)),
                end: Some(format!("3#{}", h3)),
                lines: vec!["x".into(), "y".into(), "z".into()],
            }],
        )
        .unwrap();
        assert_eq!(result, "a\nx\ny\nz\nd");
    }

    #[test]
    fn apply_append_after_line() {
        let content = "a\nb\nc";
        let file_lines: Vec<&str> = content.split('\n').collect();
        let h1 = compute_line_hash(1, file_lines[0]);
        let result = apply_hashline_edits(
            content,
            &[HashlineEditOp {
                op: "append".into(),
                pos: Some(format!("1#{}", h1)),
                end: None,
                lines: vec!["inserted".into()],
            }],
        )
        .unwrap();
        assert_eq!(result, "a\ninserted\nb\nc");
    }

    #[test]
    fn apply_prepend_before_line() {
        let content = "a\nb\nc";
        let file_lines: Vec<&str> = content.split('\n').collect();
        let h2 = compute_line_hash(2, file_lines[1]);
        let result = apply_hashline_edits(
            content,
            &[HashlineEditOp {
                op: "prepend".into(),
                pos: Some(format!("2#{}", h2)),
                end: None,
                lines: vec!["before_b".into()],
            }],
        )
        .unwrap();
        assert_eq!(result, "a\nbefore_b\nb\nc");
    }

    #[test]
    fn apply_deduplicates_identical_ops() {
        let content = "a\nb\nc";
        let file_lines: Vec<&str> = content.split('\n').collect();
        let h1 = compute_line_hash(1, file_lines[0]);
        let op = HashlineEditOp {
            op: "replace".into(),
            pos: Some(format!("1#{}", h1)),
            end: None,
            lines: vec!["replaced".into()],
        };
        let result = apply_hashline_edits(content, &[op.clone(), op]).unwrap();
        assert_eq!(result, "replaced\nb\nc");
    }

    #[test]
    fn apply_fails_on_stale_ref() {
        let content = "fn foo() {\n}";
        let err = apply_hashline_edits(
            content,
            &[HashlineEditOp {
                op: "replace".into(),
                pos: Some("1#ZZ".to_string()),
                end: None,
                lines: vec!["fn bar() {".into()],
            }],
        )
        .unwrap_err();
        assert!(err.to_string().contains(">>>"));
    }

    #[test]
    fn apply_multiple_edits_bottom_up() {
        // Two non-overlapping replacements: lines 1 and 3
        let content = "a\nb\nc\nd";
        let file_lines: Vec<&str> = content.split('\n').collect();
        let h1 = compute_line_hash(1, file_lines[0]);
        let h3 = compute_line_hash(3, file_lines[2]);
        let result = apply_hashline_edits(
            content,
            &[
                HashlineEditOp { op: "replace".into(), pos: Some(format!("1#{}", h1)), end: None, lines: vec!["A".into()] },
                HashlineEditOp { op: "replace".into(), pos: Some(format!("3#{}", h3)), end: None, lines: vec!["C".into()] },
            ],
        )
        .unwrap();
        assert_eq!(result, "A\nb\nC\nd");
    }
}
