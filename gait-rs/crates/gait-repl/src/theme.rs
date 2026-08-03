use std::time::Duration;

/// Checks once (cached) whether the terminal supports 24-bit truecolor.
/// Apple Terminal does not; iTerm2, Ghostty, WezTerm, etc. do.
pub fn supports_truecolor() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        matches!(
            std::env::var("COLORTERM").as_deref(),
            Ok("truecolor") | Ok("24bit")
        )
    })
}

/// Maps an RGB color to the nearest xterm-256 palette index.
/// Used as a fallback when the terminal doesn't support 24-bit color.
pub fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    if r == g && g == b {
        if r < 8 {
            return 16;
        }
        if r > 248 {
            return 231;
        }
        return (((r as u16 - 8) / 10) + 232) as u8;
    }
    let ri = (r as u16 * 5 + 127) / 255;
    let gi = (g as u16 * 5 + 127) / 255;
    let bi = (b as u16 * 5 + 127) / 255;
    (16 + 36 * ri + 6 * gi + bi) as u8
}

#[derive(Debug, Clone, Copy)]
pub struct Color(pub u8, pub u8, pub u8);

impl Color {
    pub fn fg(&self) -> String {
        if supports_truecolor() {
            format!("\x1b[38;2;{};{};{}m", self.0, self.1, self.2)
        } else {
            format!("\x1b[38;5;{}m", rgb_to_ansi256(self.0, self.1, self.2))
        }
    }

    pub fn bg(&self) -> String {
        if supports_truecolor() {
            format!("\x1b[48;2;{};{};{}m", self.0, self.1, self.2)
        } else {
            format!("\x1b[48;5;{}m", rgb_to_ansi256(self.0, self.1, self.2))
        }
    }
}

pub const RESET: &str = "\x1b[0m";
pub const DIM: &str = "\x1b[2m";
/// Explicit mid-gray using 256-color (index 242 ≈ #6c6c6c). Unlike DIM, this
/// renders the same regardless of the terminal's ANSI color theme.
pub const DIM_FG: &str = "\x1b[38;5;242m";

#[derive(Debug, Clone, Copy)]
pub struct ColorPair {
    pub dark: Color,
    pub light: Color,
}

pub mod palette {
    use super::{Color, ColorPair};

    pub const ACCENT: ColorPair = ColorPair {
        dark: Color(0x00, 0xC3, 0xFF),
        light: Color(0x00, 0x62, 0x80),
    };
    pub const SUCCESS: ColorPair = ColorPair {
        dark: Color(0x00, 0xFF, 0x41),
        light: Color(0x00, 0x7A, 0x20),
    };
    pub const DANGER: ColorPair = ColorPair {
        dark: Color(0xFF, 0x33, 0x33),
        light: Color(0xCC, 0x00, 0x00),
    };
    pub const WARNING: ColorPair = ColorPair {
        dark: Color(0xFF, 0xB1, 0x00),
        light: Color(0xA0, 0x60, 0x00),
    };
    pub const USER_MSG: ColorPair = ColorPair {
        dark: Color(0x77, 0x99, 0xBB),
        light: Color(0x55, 0x6B, 0x85),
    };
    /// Chrome/framing grey (gutters, borders, status text, prompt labels).
    /// Symmetric around DIM_FG's #6c6c6c (108,108,108): lighter on dark
    /// backgrounds, darker on light backgrounds, so it stays visibly distinct
    /// from the theme-independent grey used for diff context lines.
    pub const MUTED: ColorPair = ColorPair {
        dark: Color(0x94, 0x94, 0x94),
        light: Color(0x44, 0x44, 0x44),
    };

    // ── Diff ──────────────────────────────────────────────────────────
    pub const DIFF_ADDED: ColorPair = ColorPair {
        dark: Color(0x4f, 0xd6, 0xbe),
        light: Color(0x1e, 0x72, 0x5c),
    };
    pub const DIFF_REMOVED: ColorPair = ColorPair {
        dark: Color(0xc5, 0x3b, 0x53),
        light: Color(0xc5, 0x3b, 0x53),
    };
    pub const DIFF_ADDED_BG: ColorPair = ColorPair {
        dark: Color(0x20, 0x30, 0x3b),
        light: Color(0xd5, 0xe5, 0xd5),
    };
    pub const DIFF_REMOVED_BG: ColorPair = ColorPair {
        dark: Color(0x37, 0x22, 0x2c),
        light: Color(0xf7, 0xd8, 0xdb),
    };
    pub const DIFF_CONTEXT: ColorPair = ColorPair {
        dark: Color(0x82, 0x8b, 0xb8),
        light: Color(0x70, 0x86, 0xb5),
    };

    // ── Markdown (reserved for future termimad skin wiring) ───────────
    pub const MD_HEADING: ColorPair = ColorPair {
        dark: Color(0x9d, 0x7c, 0xd8),
        light: Color(0x7b, 0x5b, 0xb6),
    };
    pub const MD_CODE: ColorPair = ColorPair {
        dark: Color(0x7f, 0xd8, 0x8f),
        light: Color(0x3d, 0x9a, 0x57),
    };
    pub const MD_STRONG: ColorPair = ColorPair {
        dark: Color(0xf5, 0xa7, 0x42),
        light: Color(0xd6, 0x8c, 0x27),
    };
    pub const MD_EMPH: ColorPair = ColorPair {
        dark: Color(0xe5, 0xc0, 0x7b),
        light: Color(0xb0, 0x85, 0x1f),
    };
    pub const MD_LINK: ColorPair = ColorPair {
        dark: Color(0xfa, 0xb2, 0x83),
        light: Color(0x3b, 0x7d, 0xd8),
    };

    // ── Syntax highlighting (syntect scopes) ────────────────────────────
    pub const SYNTAX_COMMENT: ColorPair = ColorPair {
        dark: Color(0x80, 0x80, 0x80),
        light: Color(0x8a, 0x8a, 0x8a),
    };
    pub const SYNTAX_KEYWORD: ColorPair = ColorPair {
        dark: Color(0x9d, 0x7c, 0xd8),
        light: Color(0xd6, 0x8c, 0x27),
    };
    pub const SYNTAX_FUNCTION: ColorPair = ColorPair {
        dark: Color(0xfa, 0xb2, 0x83),
        light: Color(0x3b, 0x7d, 0xd8),
    };
    pub const SYNTAX_VARIABLE: ColorPair = ColorPair {
        dark: Color(0xe0, 0x6c, 0x75),
        light: Color(0xd1, 0x38, 0x3d),
    };
    pub const SYNTAX_STRING: ColorPair = ColorPair {
        dark: Color(0x7f, 0xd8, 0x8f),
        light: Color(0x3d, 0x9a, 0x57),
    };
    pub const SYNTAX_NUMBER: ColorPair = ColorPair {
        dark: Color(0xf5, 0xa7, 0x42),
        light: Color(0xd6, 0x8c, 0x27),
    };
    pub const SYNTAX_TYPE: ColorPair = ColorPair {
        dark: Color(0xe5, 0xc0, 0x7b),
        light: Color(0xb0, 0x85, 0x1f),
    };
    pub const SYNTAX_OPERATOR: ColorPair = ColorPair {
        dark: Color(0x56, 0xb6, 0xc2),
        light: Color(0x31, 0x87, 0x95),
    };
    pub const SYNTAX_PUNCTUATION: ColorPair = ColorPair {
        dark: Color(0xee, 0xee, 0xee),
        light: Color(0x1a, 0x1a, 0x1a),
    };
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub accent: Color,
    pub success: Color,
    pub danger: Color,
    pub warning: Color,
    pub user_msg: Color,
    pub muted: Color,
    pub diff_added: Color,
    pub diff_removed: Color,
    pub diff_added_bg: Color,
    pub diff_removed_bg: Color,
    pub diff_context: Color,
    pub md_heading: Color,
    pub md_code: Color,
    pub md_strong: Color,
    pub md_emph: Color,
    pub md_link: Color,
    pub syntax_comment: Color,
    pub syntax_keyword: Color,
    pub syntax_function: Color,
    pub syntax_variable: Color,
    pub syntax_string: Color,
    pub syntax_number: Color,
    pub syntax_type: Color,
    pub syntax_operator: Color,
    pub syntax_punctuation: Color,
    pub is_dark: bool,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            accent: palette::ACCENT.dark,
            success: palette::SUCCESS.dark,
            danger: palette::DANGER.dark,
            warning: palette::WARNING.dark,
            user_msg: palette::USER_MSG.dark,
            muted: palette::MUTED.dark,
            diff_added: palette::DIFF_ADDED.dark,
            diff_removed: palette::DIFF_REMOVED.dark,
            diff_added_bg: palette::DIFF_ADDED_BG.dark,
            diff_removed_bg: palette::DIFF_REMOVED_BG.dark,
            diff_context: palette::DIFF_CONTEXT.dark,
            md_heading: palette::MD_HEADING.dark,
            md_code: palette::MD_CODE.dark,
            md_strong: palette::MD_STRONG.dark,
            md_emph: palette::MD_EMPH.dark,
            md_link: palette::MD_LINK.dark,
            syntax_comment: palette::SYNTAX_COMMENT.dark,
            syntax_keyword: palette::SYNTAX_KEYWORD.dark,
            syntax_function: palette::SYNTAX_FUNCTION.dark,
            syntax_variable: palette::SYNTAX_VARIABLE.dark,
            syntax_string: palette::SYNTAX_STRING.dark,
            syntax_number: palette::SYNTAX_NUMBER.dark,
            syntax_type: palette::SYNTAX_TYPE.dark,
            syntax_operator: palette::SYNTAX_OPERATOR.dark,
            syntax_punctuation: palette::SYNTAX_PUNCTUATION.dark,
            is_dark: true,
        }
    }

    pub fn light() -> Self {
        Self {
            accent: palette::ACCENT.light,
            success: palette::SUCCESS.light,
            danger: palette::DANGER.light,
            warning: palette::WARNING.light,
            user_msg: palette::USER_MSG.light,
            muted: palette::MUTED.light,
            diff_added: palette::DIFF_ADDED.light,
            diff_removed: palette::DIFF_REMOVED.light,
            diff_added_bg: palette::DIFF_ADDED_BG.light,
            diff_removed_bg: palette::DIFF_REMOVED_BG.light,
            diff_context: palette::DIFF_CONTEXT.light,
            md_heading: palette::MD_HEADING.light,
            md_code: palette::MD_CODE.light,
            md_strong: palette::MD_STRONG.light,
            md_emph: palette::MD_EMPH.light,
            md_link: palette::MD_LINK.light,
            syntax_comment: palette::SYNTAX_COMMENT.light,
            syntax_keyword: palette::SYNTAX_KEYWORD.light,
            syntax_function: palette::SYNTAX_FUNCTION.light,
            syntax_variable: palette::SYNTAX_VARIABLE.light,
            syntax_string: palette::SYNTAX_STRING.light,
            syntax_number: palette::SYNTAX_NUMBER.light,
            syntax_type: palette::SYNTAX_TYPE.light,
            syntax_operator: palette::SYNTAX_OPERATOR.light,
            syntax_punctuation: palette::SYNTAX_PUNCTUATION.light,
            is_dark: false,
        }
    }

    pub fn detect() -> Self {
        match termbg::theme(Duration::from_millis(100)) {
            Ok(termbg::Theme::Light) => Self::light(),
            _ => Self::dark(),
        }
    }
}
