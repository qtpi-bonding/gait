//! Converts captured raw ANSI bytes into a human/agent-readable transcript:
//! each theme-slot color (accent/success/danger/warning/user_msg/muted) is
//! replaced by a semantic `<slot>...</slot>` tag instead of its escape code,
//! and cursor/erase control codes collapse to short markers. Colors the
//! current theme fixture can't produce are left as raw escape text — a
//! mismatch there means this table is stale, not something to guess at.
use crate::theme::{rgb_to_ansi256, Theme, DIM_FG, RESET};

struct SlotColor {
    name: &'static str,
    truecolor: String,
    fallback256: String,
}

/// Both encodings a theme color can appear as — truecolor and its 256-color
/// fallback — computed directly from the RGB triple, not via `Color::fg()`
/// (which depends on `supports_truecolor()`'s process-lifetime cache and may
/// have already resolved one way or the other before we get a turn).
fn slot_table(theme: &Theme) -> Vec<SlotColor> {
    let mk_fg = |name: &'static str, c: crate::theme::Color| SlotColor {
        name,
        truecolor: format!("\x1b[38;2;{};{};{}m", c.0, c.1, c.2),
        fallback256: format!("\x1b[38;5;{}m", rgb_to_ansi256(c.0, c.1, c.2)),
    };
    let mk_bg = |name: &'static str, c: crate::theme::Color| SlotColor {
        name,
        truecolor: format!("\x1b[48;2;{};{};{}m", c.0, c.1, c.2),
        fallback256: format!("\x1b[48;5;{}m", rgb_to_ansi256(c.0, c.1, c.2)),
    };
    vec![
        mk_fg("accent", theme.accent),
        mk_fg("success", theme.success),
        mk_fg("danger", theme.danger),
        mk_fg("warning", theme.warning),
        mk_fg("user_msg", theme.user_msg),
        mk_fg("muted", theme.muted),
        mk_fg("diff_added", theme.diff_added),
        mk_fg("diff_removed", theme.diff_removed),
        mk_bg("diff_added_bg", theme.diff_added_bg),
        mk_bg("diff_removed_bg", theme.diff_removed_bg),
        mk_fg("diff_context", theme.diff_context),
        mk_fg("md_heading", theme.md_heading),
        mk_fg("md_code", theme.md_code),
        mk_fg("md_strong", theme.md_strong),
        mk_fg("md_emph", theme.md_emph),
        mk_fg("md_link", theme.md_link),
        mk_fg("syntax_comment", theme.syntax_comment),
        mk_fg("syntax_keyword", theme.syntax_keyword),
        mk_fg("syntax_function", theme.syntax_function),
        mk_fg("syntax_variable", theme.syntax_variable),
        mk_fg("syntax_string", theme.syntax_string),
        mk_fg("syntax_number", theme.syntax_number),
        mk_fg("syntax_type", theme.syntax_type),
        mk_fg("syntax_operator", theme.syntax_operator),
        mk_fg("syntax_punctuation", theme.syntax_punctuation),
    ]
}

pub(crate) fn annotate(bytes: &[u8], theme: &Theme) -> String {
    let input = String::from_utf8_lossy(bytes).into_owned();
    let table = slot_table(theme);
    let mut out = String::with_capacity(input.len());
    let mut open: Option<&'static str> = None;
    let mut rest: &str = &input;

    while let Some(pos) = rest.find(['\x1b', '\r']) {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];

        if rest.starts_with('\r') {
            out.push_str("[clear-line]");
            rest = &rest[1..];
            continue;
        }

        if let Some((name, len)) = table.iter().find_map(|c| {
            if rest.starts_with(c.truecolor.as_str()) {
                Some((c.name, c.truecolor.len()))
            } else if rest.starts_with(c.fallback256.as_str()) {
                Some((c.name, c.fallback256.len()))
            } else {
                None
            }
        }) {
            if let Some(prev) = open.take() {
                out.push_str(&format!("</{prev}>"));
            }
            out.push_str(&format!("<{name}>"));
            open = Some(name);
            rest = &rest[len..];
        } else if rest.starts_with(DIM_FG) {
            if let Some(prev) = open.take() {
                out.push_str(&format!("</{prev}>"));
            }
            out.push_str("<dim>");
            open = Some("dim");
            rest = &rest[DIM_FG.len()..];
        } else if rest.starts_with(RESET) {
            if let Some(prev) = open.take() {
                out.push_str(&format!("</{prev}>"));
            }
            rest = &rest[RESET.len()..];
        } else if rest.starts_with("\x1b[2K") {
            out.push_str("[clear-line]");
            rest = &rest[4..];
        } else if rest.starts_with("\x1b[K") {
            out.push_str("[clear-line]");
            rest = &rest[3..];
        } else {
            // Unrecognized escape: emit the ESC byte raw and keep scanning
            // from the next byte, so a mismatch never gets silently dropped
            // or mis-tagged, and we never loop on the same position.
            out.push('\x1b');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    if let Some(prev) = open.take() {
        out.push_str(&format!("</{prev}>"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truecolor_danger_gets_tagged() {
        let theme = Theme::dark();
        let bytes = format!(
            "\x1b[38;2;{};{};{}mboom{}",
            theme.danger.0, theme.danger.1, theme.danger.2, RESET
        );
        assert_eq!(annotate(bytes.as_bytes(), &theme), "<danger>boom</danger>");
    }

    #[test]
    fn fallback_256_danger_gets_same_tag() {
        let theme = Theme::dark();
        let code = rgb_to_ansi256(theme.danger.0, theme.danger.1, theme.danger.2);
        let bytes = format!("\x1b[38;5;{code}mboom{RESET}");
        assert_eq!(annotate(bytes.as_bytes(), &theme), "<danger>boom</danger>");
    }

    #[test]
    fn light_theme_danger_gets_tagged() {
        let theme = Theme::light();
        let bytes = format!(
            "\x1b[38;2;{};{};{}mboom{}",
            theme.danger.0, theme.danger.1, theme.danger.2, RESET
        );
        assert_eq!(annotate(bytes.as_bytes(), &theme), "<danger>boom</danger>");
    }

    #[test]
    fn dim_fg_literal_gets_tagged() {
        let theme = Theme::dark();
        let bytes = format!("{DIM_FG}context line{RESET}");
        assert_eq!(
            annotate(bytes.as_bytes(), &theme),
            "<dim>context line</dim>"
        );
    }

    #[test]
    fn unrecognized_sgr_passes_through_raw() {
        let theme = Theme::dark();
        let bytes = "\x1b[1mbold\x1b[0m";
        assert_eq!(annotate(bytes.as_bytes(), &theme), "\x1b[1mbold");
    }

    #[test]
    fn unterminated_color_closes_at_end_of_stream() {
        let theme = Theme::dark();
        let bytes = format!(
            "\x1b[38;2;{};{};{}munterminated",
            theme.warning.0, theme.warning.1, theme.warning.2
        );
        assert_eq!(
            annotate(bytes.as_bytes(), &theme),
            "<warning>unterminated</warning>"
        );
    }

    #[test]
    fn clear_line_codes_collapse_to_marker() {
        let theme = Theme::dark();
        let bytes = "\r\x1b[2Kspinner";
        assert_eq!(
            annotate(bytes.as_bytes(), &theme),
            "[clear-line][clear-line]spinner"
        );
    }
}
