//! Syntax highlighting for fenced code blocks using syntect.
//! Maps syntect TextMate scopes to the gait theme palette.

use crate::theme::Color;
use std::sync::OnceLock;
use syntect::highlighting::{Color as SColor, Theme, ThemeItem, ThemeSettings};
use syntect::parsing::SyntaxSet;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// Build a syntect Theme from the gait REPL palette.
fn build_syntect_theme(theme: &crate::theme::Theme) -> Theme {
    let to_scolor = |c: &Color| SColor {
        r: c.0,
        g: c.1,
        b: c.2,
        a: 0xFF,
    };

    let item = |scope: &str, fg: SColor| ThemeItem {
        scope: scope.parse().expect("valid scope selector"),
        style: syntect::highlighting::StyleModifier {
            foreground: Some(fg),
            background: None,
            font_style: None,
        },
    };

    Theme {
        name: Some("gait".to_string()),
        author: Some("gait".to_string()),
        settings: ThemeSettings::default(),
        scopes: vec![
            item("comment", to_scolor(&theme.syntax_comment)),
            item("keyword", to_scolor(&theme.syntax_keyword)),
            item(
                "entity.name.function, support.function",
                to_scolor(&theme.syntax_function),
            ),
            item("variable", to_scolor(&theme.syntax_variable)),
            item("string", to_scolor(&theme.syntax_string)),
            item("constant.numeric", to_scolor(&theme.syntax_number)),
            item(
                "storage.type, entity.name.type",
                to_scolor(&theme.syntax_type),
            ),
            item("keyword.operator", to_scolor(&theme.syntax_operator)),
            item("punctuation", to_scolor(&theme.syntax_punctuation)),
        ],
    }
}

/// Truncate a line to `width` display columns, appending `…` if truncated.
fn truncate_line(line: &str, width: usize) -> String {
    if line.width() <= width {
        return line.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in line.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > width.saturating_sub(1) {
            out.push('…');
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

/// Highlight a code block. Returns `None` if the language isn't known.
/// The output is a string with embedded ANSI true-color (or 256-color)
/// escape sequences; each line is preserved without a trailing newline.
///
/// Long lines are truncated to `width` display columns (accounting for
/// the gutter the caller will prepend).
pub fn highlight_block(
    code: &str,
    lang: &str,
    theme: &crate::theme::Theme,
    width: Option<usize>,
) -> Option<String> {
    let ss = syntax_set();
    let syntax = ss.find_syntax_by_token(lang)?;
    let syntect_theme = build_syntect_theme(theme);

    let mut h = syntect::easy::HighlightLines::new(syntax, &syntect_theme);

    let mut out = String::with_capacity(code.len() * 2);
    for line in code.lines() {
        let line = width.map_or_else(|| line.to_string(), |w| truncate_line(line, w));
        let highlighted = h.highlight_line(&line, ss).ok()?;
        for (style, text) in highlighted {
            let c = &style.foreground;
            let ansi = if crate::theme::supports_truecolor() {
                format!("\x1b[38;2;{};{};{}m", c.r, c.g, c.b)
            } else {
                let idx = crate::theme::rgb_to_ansi256(c.r, c.g, c.b);
                format!("\x1b[38;5;{}m", idx)
            };
            out.push_str(&ansi);
            out.push_str(text);
            out.push_str(crate::theme::RESET);
        }
        out.push('\n');
    }
    // Remove trailing newline if present, so the caller decides on line breaks.
    if out.ends_with('\n') {
        out.pop();
    }
    Some(out)
}
