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
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub accent: Color,
    pub success: Color,
    pub danger: Color,
    pub warning: Color,
    pub user_msg: Color,
    pub muted: Color,
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
