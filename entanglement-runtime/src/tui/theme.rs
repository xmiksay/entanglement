use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};
use std::hash::Hasher;

#[derive(Clone, Copy, Debug)]
pub(crate) struct RoleColors {
    pub fg: Color,
    pub bg: Color,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Theme {
    pub bar_glyph: char,
    pub ship_right: char,
    pub ship_left: char,
    pub trail_glyph: char,
    pub assistant_fg: Color,
    pub tool_req_fg: Color,
    pub tool_out_fg: Color,
    pub error_fg: Color,
    pub message_bg: Color,
    pub sidebar_bg: Color,
    pub sidebar_fg: Color,
    pub input_bg: Color,
    pub chat_margin_left: u16,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            bar_glyph: '▌',
            ship_right: '▸',
            ship_left: '◂',
            trail_glyph: '▮',
            assistant_fg: Color::Cyan,
            tool_req_fg: Color::Cyan,
            tool_out_fg: Color::Gray,
            error_fg: Color::Red,
            message_bg: Color::Rgb(30, 30, 35),
            sidebar_bg: Color::Rgb(26, 26, 33),
            sidebar_fg: Color::Cyan,
            input_bg: Color::Rgb(26, 26, 33),
            chat_margin_left: 2,
        }
    }
}

impl Theme {
    pub fn user_colors(self, profile_color: Color) -> RoleColors {
        RoleColors {
            fg: profile_color,
            bg: self.message_bg,
        }
    }

    pub fn user_input_colors(self, profile_color: Color) -> RoleColors {
        RoleColors {
            fg: profile_color,
            bg: self.input_bg,
        }
    }

    pub fn assistant_colors(&self) -> RoleColors {
        RoleColors {
            fg: self.assistant_fg,
            bg: self.message_bg,
        }
    }

    pub fn reasoning_colors(&self) -> RoleColors {
        RoleColors {
            fg: Color::Gray,
            bg: self.message_bg,
        }
    }

    pub fn tool_req_colors(&self) -> RoleColors {
        RoleColors {
            fg: self.tool_req_fg,
            bg: self.message_bg,
        }
    }

    pub fn tool_out_colors(&self) -> RoleColors {
        RoleColors {
            fg: self.tool_out_fg,
            bg: self.message_bg,
        }
    }

    pub fn error_colors(&self) -> RoleColors {
        RoleColors {
            fg: self.error_fg,
            bg: self.message_bg,
        }
    }

    pub fn sidebar_colors(&self) -> RoleColors {
        RoleColors {
            fg: self.sidebar_fg,
            bg: self.sidebar_bg,
        }
    }

    pub fn decorate<'a>(self, line: Line<'a>, c: RoleColors, width: u16) -> Line<'a> {
        let content_len = line.spans.iter().map(|s| s.width() as u16).sum::<u16>();
        let remaining = width.saturating_sub(content_len).saturating_sub(3);

        let mut spans = vec![
            Span::styled(
                self.bar_glyph.to_string(),
                Style::default().fg(c.fg).bg(c.bg),
            ),
            Span::raw(" "),
        ];
        spans.extend(line.spans);
        for _ in 0..remaining {
            spans.push(Span::raw(" "));
        }

        Line::from(spans).bg(c.bg)
    }
}

pub(crate) fn darken(color: Color, factor: f32) -> Color {
    match color {
        Color::Rgb(r, g, b) => {
            let r = (r as f32 * (1.0 - factor)).max(0.0) as u8;
            let g = (g as f32 * (1.0 - factor)).max(0.0) as u8;
            let b = (b as f32 * (1.0 - factor)).max(0.0) as u8;
            Color::Rgb(r, g, b)
        }
        Color::Indexed(i) => Color::Indexed(i),
        _ => color,
    }
}

pub(crate) fn hash_profile_color(name: &str) -> Color {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(name, &mut hasher);
    let hash = hasher.finish();

    let hue = (hash % 360) as u8;
    let saturation = 70;
    let value = 90;

    hsv_to_rgb(hue, saturation, value)
}

fn hsv_to_rgb(h: u8, s: u8, v: u8) -> Color {
    let h = h as f64 / 360.0;
    let s = s as f64 / 100.0;
    let v = v as f64 / 100.0;

    let c = v * s;
    let x = c * (1.0 - ((h * 6.0) % 2.0 - 1.0).abs());
    let m = v - c;

    let (r, g, b) = if h < 1.0 / 6.0 {
        (c, x, 0.0)
    } else if h < 2.0 / 6.0 {
        (x, c, 0.0)
    } else if h < 3.0 / 6.0 {
        (0.0, c, x)
    } else if h < 4.0 / 6.0 {
        (0.0, x, c)
    } else if h < 5.0 / 6.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };

    Color::Rgb(
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_darken_rgb() {
        let original = Color::Rgb(100, 150, 200);
        let darkened = darken(original, 0.15);
        match darkened {
            Color::Rgb(r, g, b) => {
                assert!(r < 100);
                assert!(g < 150);
                assert!(b < 200);
            }
            _ => panic!("Expected Color::Rgb"),
        }
    }

    #[test]
    fn test_theme_default() {
        let theme = Theme::default();
        assert_eq!(theme.bar_glyph, '▌');
        assert_eq!(theme.ship_right, '▸');
        assert_eq!(theme.ship_left, '◂');
        assert_eq!(theme.trail_glyph, '▮');
        assert_eq!(theme.assistant_fg, Color::Cyan);
        assert_eq!(theme.message_bg, Color::Rgb(30, 30, 35));
    }

    #[test]
    fn test_theme_colors() {
        let theme = Theme::default();
        let assistant = theme.assistant_colors();
        assert_eq!(assistant.fg, Color::Cyan);
        assert_eq!(assistant.bg, Color::Rgb(30, 30, 35));
    }

    #[test]
    fn test_decorate_line() {
        let theme = Theme::default();
        let line = Line::from("test");
        let assistant = theme.assistant_colors();
        let decorated = theme.decorate(line, assistant, 20);
        assert!(decorated.spans.len() >= 3);
        assert_eq!(decorated.spans[0].content.as_ref(), "▌");
        assert_eq!(decorated.spans[1].content.as_ref(), " ");
        assert!(decorated.spans.iter().any(|s| s.content.as_ref() == "test"));
    }
}
