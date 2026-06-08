use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct Color(pub u8, pub u8, pub u8);

impl Color {
    pub fn fg(&self) -> String {
        format!("\x1b[38;2;{};{};{}m", self.0, self.1, self.2)
    }
}

pub const RESET: &str = "\x1b[0m";
pub const DIM: &str = "\x1b[2m";

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
    pub const META: ColorPair = ColorPair {
        dark: Color(0x88, 0x99, 0xAA),
        light: Color(0x6B, 0x72, 0x80),
    };
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub accent: Color,
    pub success: Color,
    pub danger: Color,
    pub warning: Color,
    pub meta: Color,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            accent: palette::ACCENT.dark,
            success: palette::SUCCESS.dark,
            danger: palette::DANGER.dark,
            warning: palette::WARNING.dark,
            meta: palette::META.dark,
        }
    }

    pub fn light() -> Self {
        Self {
            accent: palette::ACCENT.light,
            success: palette::SUCCESS.light,
            danger: palette::DANGER.light,
            warning: palette::WARNING.light,
            meta: palette::META.light,
        }
    }

    pub fn detect() -> Self {
        match termbg::theme(Duration::from_millis(100)) {
            Ok(termbg::Theme::Light) => Self::light(),
            _ => Self::dark(),
        }
    }
}
