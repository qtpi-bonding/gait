//! Hashline — content-hash-anchored line references for reliable file edits.
//!
//! Read output format:  `42#NB|fn render_help() {`
//! Edit ref format:     `42#NB`
//!
//! Hash is xxHash32(trimmed_content, seed) % 256 mapped to a 2-char code.
//! Seed 0 for lines with alphanumeric content (position-independent).
//! Seed = line number for structural lines (`{`, `}`, blank) so identical
//! structural characters at different positions get distinct hashes.
//!
//! Ported and adapted from oh-my-openagent by code-yeongyu (MIT License)
//! <https://github.com/code-yeongyu/oh-my-openagent>
//!
//! The hash-anchored edit scheme, xxHash32 implementation, and autocorrect
//! normalization layer are all derived from that project's TypeScript source.

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
    #[serde(deserialize_with = "de_lines")]
    pub lines: Vec<String>,
}

/// Tolerant deserializer for `lines`. Weak models (notably MiniMax) emit the
/// array in non-canonical shapes; accept all of them so a valid edit isn't
/// rejected with "invalid type: map, expected a sequence":
///   - `["a","b"]`            → as-is (canonical)
///   - `"a\nb"`               → split on newlines
///   - `{"item":["a","b"]}`   → unwrapped (MiniMax wraps arrays under item/items/lines/value)
fn de_lines<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Lines {
        Vec(Vec<String>),
        Str(String),
        Wrapped(std::collections::BTreeMap<String, Vec<String>>),
    }
    match Lines::deserialize(d)? {
        Lines::Vec(v) => Ok(v),
        Lines::Str(s) => Ok(s.split('\n').map(|x| x.to_string()).collect()),
        Lines::Wrapped(mut m) => {
            for k in ["item", "items", "lines", "value", "values"] {
                if let Some(v) = m.remove(k) {
                    return Ok(v);
                }
            }
            // Any single wrapped array key.
            m.into_values()
                .next()
                .ok_or_else(|| serde::de::Error::custom("`lines` object had no array value"))
        }
    }
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

// ── Autocorrect ───────────────────────────────────────────────────────────────
// Silent fixups for common model mistakes in replacement payloads.
// Every function is a no-op when the model sends correct content.

fn has_hashline_prefix(line: &str) -> bool {
    let Some(hash_pos) = line.find('#') else { return false };
    let before = &line[..hash_pos];
    if before.is_empty() || !before.bytes().all(|b| b.is_ascii_digit()) { return false; }
    let after = &line[hash_pos + 1..];
    after.len() >= 3 && is_valid_hash(&after[..2]) && after.as_bytes()[2] == b'|'
}

/// Strip accidental `N#XY|` or `+` prefixes if ≥50% of non-empty replacement lines carry them.
/// Models sometimes copy-paste directly from read output and forget to remove the anchor prefix.
fn strip_line_prefixes(lines: Vec<String>) -> Vec<String> {
    let non_empty: usize = lines.iter().filter(|l| !l.trim().is_empty()).count();
    if non_empty == 0 { return lines; }
    let prefixed: usize = lines.iter()
        .filter(|l| !l.trim().is_empty() && (has_hashline_prefix(l) || l.starts_with('+')))
        .count();
    if prefixed * 2 < non_empty { return lines; }
    lines.into_iter().map(|l| {
        if has_hashline_prefix(&l) {
            l.splitn(2, '|').nth(1).unwrap_or(&l).to_string()
        } else if l.starts_with('+') {
            l[1..].to_string()
        } else {
            l
        }
    }).collect()
}

/// Strip the anchor line if the model echoed it as the first line of an `append` payload.
fn strip_insert_anchor_echo(anchor_content: &str, mut lines: Vec<String>) -> Vec<String> {
    if lines.first().map(|l| l.trim()) == Some(anchor_content.trim()) {
        lines.remove(0);
    }
    lines
}

/// Strip the anchor line if the model echoed it as the last line of a `prepend` payload.
fn strip_insert_before_echo(anchor_content: &str, mut lines: Vec<String>) -> Vec<String> {
    if lines.last().map(|l| l.trim()) == Some(anchor_content.trim()) {
        lines.pop();
    }
    lines
}

/// Strip the line just before or after the replaced range if the model included it as context.
fn strip_range_boundary_echo(
    file_lines: &[String],
    start_line: usize, // 1-indexed inclusive
    end_line: usize,   // 1-indexed inclusive
    mut new_lines: Vec<String>,
) -> Vec<String> {
    if start_line > 1 {
        let before = file_lines[start_line - 2].trim();
        if new_lines.first().map(|l| l.trim()) == Some(before) {
            new_lines.remove(0);
        }
    }
    if end_line < file_lines.len() {
        let after = file_lines[end_line].trim(); // 0-indexed: file_lines[end_line] is one past end
        if new_lines.last().map(|l| l.trim()) == Some(after) {
            new_lines.pop();
        }
    }
    new_lines
}

/// Restore leading whitespace the model dropped from a replacement line.
fn restore_leading_indent(template: &str, new_line: String) -> String {
    let indent: String = template.chars().take_while(|c| c.is_whitespace()).collect();
    if indent.is_empty() { return new_line; }
    let new_indent: String = new_line.chars().take_while(|c| c.is_whitespace()).collect();
    if new_indent.is_empty() && !new_line.trim().is_empty() {
        format!("{}{}", indent, new_line)
    } else {
        new_line
    }
}

/// Restore per-line indentation using original lines as templates.
/// When counts match, restores each line; otherwise just restores the first.
fn autocorrect_replacement_lines(original_lines: &[String], mut new_lines: Vec<String>) -> Vec<String> {
    if new_lines.is_empty() || original_lines.is_empty() { return new_lines; }
    if original_lines.len() == new_lines.len() {
        new_lines = new_lines.into_iter()
            .zip(original_lines.iter())
            .map(|(n, o)| restore_leading_indent(o, n))
            .collect();
    } else {
        let first = new_lines.remove(0);
        new_lines.insert(0, restore_leading_indent(&original_lines[0], first));
    }
    new_lines
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
                let original_lines: Vec<String> = owned[start - 1..end].to_vec();
                let new_lines = strip_line_prefixes(op.lines.clone());
                let new_lines = strip_range_boundary_echo(&owned, start, end, new_lines);
                let new_lines = autocorrect_replacement_lines(&original_lines, new_lines);
                owned.splice((start - 1)..end, new_lines.into_iter());
            }
            "append" => {
                match op.pos.as_deref().map(parse_line_ref) {
                    Some(Ok(lr)) => {
                        let anchor_content = owned[lr.line - 1].clone();
                        let new_lines = strip_line_prefixes(op.lines.clone());
                        let new_lines = strip_insert_anchor_echo(&anchor_content, new_lines);
                        for (i, line) in new_lines.iter().enumerate() {
                            owned.insert(lr.line + i, line.clone());
                        }
                    }
                    None => owned.extend(strip_line_prefixes(op.lines.clone())),
                    Some(Err(e)) => return Err(e),
                }
            }
            "prepend" => {
                match op.pos.as_deref().map(parse_line_ref) {
                    Some(Ok(lr)) => {
                        let anchor_content = owned[lr.line - 1].clone();
                        let new_lines = strip_line_prefixes(op.lines.clone());
                        let new_lines = strip_insert_before_echo(&anchor_content, new_lines);
                        let idx = lr.line - 1;
                        for (i, line) in new_lines.iter().enumerate() {
                            owned.insert(idx + i, line.clone());
                        }
                    }
                    None => {
                        let mut prefix = strip_line_prefixes(op.lines.clone());
                        prefix.extend(owned);
                        owned = prefix;
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
    fn autocorrect_strips_hashline_prefixes() {
        let lines = vec![
            "1#NB|fn foo() {".to_string(),
            "2#ZP|    bar();".to_string(),
            "3#QR|}".to_string(),
        ];
        let result = strip_line_prefixes(lines);
        assert_eq!(result, vec!["fn foo() {", "    bar();", "}"]);
    }

    #[test]
    fn autocorrect_strips_diff_plus_prefixes() {
        let lines = vec!["+fn foo() {".to_string(), "+    bar();".to_string()];
        let result = strip_line_prefixes(lines);
        assert_eq!(result, vec!["fn foo() {", "    bar();"]);
    }

    #[test]
    fn autocorrect_does_not_strip_when_minority_prefixed() {
        let lines = vec!["1#NB|fn foo() {".to_string(), "normal line".to_string(), "another".to_string()];
        let result = strip_line_prefixes(lines.clone());
        assert_eq!(result, lines); // unchanged — only 1/3 prefixed
    }

    #[test]
    fn autocorrect_strips_anchor_echo_for_append() {
        let anchor = "fn foo() {".to_string();
        let lines = vec!["fn foo() {".to_string(), "    // new comment".to_string()];
        let result = strip_insert_anchor_echo(&anchor, lines);
        assert_eq!(result, vec!["    // new comment"]);
    }

    #[test]
    fn autocorrect_strips_anchor_echo_for_prepend() {
        let anchor = "fn foo() {".to_string();
        let lines = vec!["    // new comment".to_string(), "fn foo() {".to_string()];
        let result = strip_insert_before_echo(&anchor, lines);
        assert_eq!(result, vec!["    // new comment"]);
    }

    #[test]
    fn autocorrect_strips_range_boundary_echo() {
        let file: Vec<String> = vec!["before".into(), "line2".into(), "line3".into(), "after".into()];
        // Replacing lines 2-3; model included "before" at start and "after" at end
        let new_lines = vec!["before".into(), "replacement".into(), "after".into()];
        let result = strip_range_boundary_echo(&file, 2, 3, new_lines);
        assert_eq!(result, vec!["replacement"]);
    }

    #[test]
    fn autocorrect_restores_indent_on_paired_replace() {
        let original = vec!["    fn foo() {".to_string(), "    }".to_string()];
        let replacement = vec!["fn bar() {".to_string(), "}".to_string()];
        let result = autocorrect_replacement_lines(&original, replacement);
        assert_eq!(result, vec!["    fn bar() {", "    }"]);
    }

    #[test]
    fn replace_autocorrects_prefixed_lines() {
        let content = "fn foo() {\n    bar();\n}";
        let file_lines: Vec<&str> = content.split('\n').collect();
        let h2 = compute_line_hash(2, file_lines[1]);
        // Model accidentally includes the N#XY| prefix in its replacement
        let result = apply_hashline_edits(
            content,
            &[HashlineEditOp {
                op: "replace".into(),
                pos: Some(format!("2#{}", h2)),
                end: None,
                lines: vec![format!("2#{}|    baz();", h2)],
            }],
        ).unwrap();
        assert!(result.contains("    baz();"), "got: {result}");
        assert!(!result.contains('#'), "prefix should have been stripped: {result}");
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
