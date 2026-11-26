use cosmic::iced::Color;

pub const SKIN_KEY: &str = "skin";
pub const SKINS: [&str; 6] = ["native", "synthwave", "cozy", "terminal", "disco", "zen"];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkinStyle {
    pub accent: Color,
    pub accent_rgb: [u8; 3],
    pub border_tint: Color,
    pub bg_tint: Color,
    pub window_bg: [u8; 3],
    pub foreground: [u8; 3],
    pub border_width: f32,
}

impl SkinStyle {
    pub fn palette(&self) -> maprisma_kit::look::Palette {
        maprisma_kit::look::Palette {
            accent: Some(self.accent_rgb),
            background: Some(self.window_bg),
            foreground: Some(self.foreground),
            red: None,
            yellow: None,
            green: None,
        }
    }
}

pub fn normalize_skin(skin: &str) -> &'static str {
    SKINS.iter().find(|s| **s == skin).copied().unwrap_or("native")
}

pub fn skin_style(skin: &str) -> Option<SkinStyle> {
    match normalize_skin(skin) {
        "synthwave" => Some(SkinStyle {
            accent: Color::from_rgb8(0xff, 0x2e, 0x93), accent_rgb: [0xff, 0x2e, 0x93],
            border_tint: Color::from_rgb8(0x00, 0xe5, 0xff), bg_tint: Color::from_rgb8(0x3a, 0x12, 0x59),
            window_bg: [0x13, 0x09, 0x24], foreground: [0xf4, 0xea, 0xff], border_width: 1.5,
        }),
        "cozy" => Some(SkinStyle {
            accent: Color::from_rgb8(0xf0, 0x9a, 0x36), accent_rgb: [0xf0, 0x9a, 0x36],
            border_tint: Color::from_rgb8(0xe0, 0x82, 0x2a), bg_tint: Color::from_rgb8(0x59, 0x30, 0x12),
            window_bg: [0x21, 0x15, 0x0e], foreground: [0xf5, 0xe8, 0xd6], border_width: 1.25,
        }),
        "terminal" => Some(SkinStyle {
            accent: Color::from_rgb8(0x2c, 0xe8, 0x68), accent_rgb: [0x2c, 0xe8, 0x68],
            border_tint: Color::from_rgb8(0x22, 0xc5, 0x5e), bg_tint: Color::from_rgb8(0x0d, 0x38, 0x1a),
            window_bg: [0x09, 0x13, 0x0b], foreground: [0xd2, 0xfc, 0xe0], border_width: 1.25,
        }),
        "disco" => Some(SkinStyle {
            accent: Color::from_rgb8(0xd9, 0x46, 0xef), accent_rgb: [0xd9, 0x46, 0xef],
            border_tint: Color::from_rgb8(0xfa, 0xcc, 0x15), bg_tint: Color::from_rgb8(0x4c, 0x16, 0x66),
            window_bg: [0x1c, 0x0c, 0x2b], foreground: [0xf8, 0xec, 0xff], border_width: 1.5,
        }),
        "zen" => Some(SkinStyle {
            accent: Color::from_rgb8(0x52, 0xb7, 0x98), accent_rgb: [0x52, 0xb7, 0x98],
            border_tint: Color::from_rgb8(0x64, 0xc2, 0xa4), bg_tint: Color::from_rgb8(0x1e, 0x38, 0x32),
            window_bg: [0x14, 0x1d, 0x1c], foreground: [0xe2, 0xf0, 0xec], border_width: 1.0,
        }),
        _ => None,
    }
}

pub fn skin_label(skin: &str) -> String {
    crate::i18n::tr(match normalize_skin(skin) {
        "synthwave" => "Synthwave", "cozy" => "Cozy", "terminal" => "Terminal",
        "disco" => "Disco", "zen" => "Zen", _ => "Native",
    }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_and_native() {
        assert_eq!(SKIN_KEY, "skin");
        assert_eq!(SKINS, ["native", "synthwave", "cozy", "terminal", "disco", "zen"]);
        for invalid in ["", "unknown", "Native", " synthwave"] {
            assert_eq!(normalize_skin(invalid), "native");
            assert_eq!(skin_style(invalid), None);
        }
        for name in SKINS { assert_eq!(normalize_skin(name), name); }
        assert_eq!(skin_style("native"), None);
    }

    #[test]
    fn exact_palettes() {
        let expected = [
            ("synthwave", [0xff, 0x2e, 0x93], [0x00, 0xe5, 0xff], [0x3a, 0x12, 0x59], [0x13, 0x09, 0x24], [0xf4, 0xea, 0xff], 1.5),
            ("cozy", [0xf0, 0x9a, 0x36], [0xe0, 0x82, 0x2a], [0x59, 0x30, 0x12], [0x21, 0x15, 0x0e], [0xf5, 0xe8, 0xd6], 1.25),
            ("terminal", [0x2c, 0xe8, 0x68], [0x22, 0xc5, 0x5e], [0x0d, 0x38, 0x1a], [0x09, 0x13, 0x0b], [0xd2, 0xfc, 0xe0], 1.25),
            ("disco", [0xd9, 0x46, 0xef], [0xfa, 0xcc, 0x15], [0x4c, 0x16, 0x66], [0x1c, 0x0c, 0x2b], [0xf8, 0xec, 0xff], 1.5),
            ("zen", [0x52, 0xb7, 0x98], [0x64, 0xc2, 0xa4], [0x1e, 0x38, 0x32], [0x14, 0x1d, 0x1c], [0xe2, 0xf0, 0xec], 1.0),
        ];
        for (name, accent, border, bg, window, fg, width) in expected {
            let style = skin_style(name).unwrap();
            assert_eq!(style.accent_rgb, accent);
            assert_eq!(style.accent, Color::from_rgb8(accent[0], accent[1], accent[2]));
            assert_eq!(style.border_tint, Color::from_rgb8(border[0], border[1], border[2]));
            assert_eq!(style.bg_tint, Color::from_rgb8(bg[0], bg[1], bg[2]));
            assert_eq!((style.window_bg, style.foreground, style.border_width), (window, fg, width));
            let palette = style.palette();
            assert_eq!((palette.accent, palette.background, palette.foreground), (Some(accent), Some(window), Some(fg)));
            assert_eq!((palette.red, palette.yellow, palette.green), (None, None, None));
            assert!(!palette.is_empty());
        }
    }

    #[test]
    fn translated_labels_exist() {
        for name in SKINS { assert!(!skin_label(name).is_empty()); }
        assert_eq!(skin_label("invalid"), skin_label("native"));
    }
}
