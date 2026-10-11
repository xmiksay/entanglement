//! The TUI's resolved theme: glyphs plus one colour per named
//! [`Slot`] (#735). Every colour a widget draws comes from [`Theme::c`] /
//! [`Theme::fg`] — the palettes themselves live in [`palette`], the slot names
//! and the `tui.theme` grammar in [`crate::config::theme`].

use ratatui::{
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
};
use std::hash::Hasher;

pub(crate) use crate::config::theme::Slot;
use crate::config::theme::{ResolvedTheme, ThemeBase};

mod palette;

#[derive(Clone, Copy, Debug)]
pub(crate) struct RoleColors {
    pub fg: Color,
    pub bg: Color,
}

/// The status line's styles (#762), one per `status.*` slot. Emphasis
/// (bold repo, dim separators, bold badge) is fixed; only the colour is
/// themeable, so it survives `NO_COLOR`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StatusTheme {
    /// `{repo}` — never shed, so it anchors the line.
    pub repo: Style,
    /// `{cwd}` / `{cwd_rel}`.
    pub path: Style,
    pub branch: Style,
    pub worktree: Style,
    /// Every other placeholder value.
    pub text: Style,
    /// Template literals (separators) and `{session}`.
    pub dim: Style,
    /// The waiting-sessions badge.
    pub attention: Style,
}

impl StatusTheme {
    fn from_palette(p: &[Color; Slot::COUNT]) -> Self {
        // `Reset` leaves `fg` unset, so the dark default compares equal to the
        // pre-#735 hardcoded styles.
        let with = |style: Style, slot: Slot| match p[slot as usize] {
            Color::Reset => style,
            c => style.fg(c),
        };
        StatusTheme {
            repo: with(Style::default().bold(), Slot::StatusRepo),
            path: with(Style::default(), Slot::StatusPath),
            branch: with(Style::default(), Slot::StatusBranch),
            worktree: with(Style::default(), Slot::StatusWorktree),
            text: with(Style::default(), Slot::StatusText),
            dim: with(Style::default().dim(), Slot::StatusDim),
            attention: with(Style::default().bold(), Slot::StatusAttention),
        }
    }
}

impl Default for StatusTheme {
    fn default() -> Self {
        Theme::default().status
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Theme {
    pub bar_glyph: char,
    pub ship_right: char,
    pub ship_left: char,
    pub trail_glyph: char,
    pub chat_margin_left: u16,
    pub status: StatusTheme,
    palette: [Color; Slot::COUNT],
    base: ThemeBase,
    no_color: bool,
}

impl Default for Theme {
    fn default() -> Self {
        Theme::resolve(
            &ResolvedTheme {
                base: ThemeBase::Dark,
                overrides: Vec::new(),
                warnings: Vec::new(),
            },
            false,
        )
    }
}

impl Theme {
    /// Build the palette: the base's colours, then the valid overrides. With
    /// `no_color` (the `NO_COLOR` convention) every slot is the terminal
    /// default and only modifiers carry emphasis.
    pub fn resolve(cfg: &ResolvedTheme, no_color: bool) -> Theme {
        let mut palette = [Color::Reset; Slot::COUNT];
        if !no_color {
            for slot in Slot::ALL {
                palette[*slot as usize] = palette::base_color(cfg.base, *slot);
            }
            for (slot, c) in &cfg.overrides {
                palette[*slot as usize] = palette::to_ratatui(*c);
            }
        }
        Theme {
            bar_glyph: '▌',
            ship_right: '▸',
            ship_left: '◂',
            trail_glyph: '▮',
            chat_margin_left: 2,
            status: StatusTheme::from_palette(&palette),
            palette,
            base: cfg.base,
            no_color,
        }
    }

    /// The colour of `slot`.
    pub fn c(&self, slot: Slot) -> Color {
        self.palette[slot as usize]
    }

    /// A style with `slot` as its foreground.
    pub fn fg(&self, slot: Slot) -> Style {
        Style::default().fg(self.c(slot))
    }

    pub fn base(&self) -> ThemeBase {
        self.base
    }

    pub fn no_color(&self) -> bool {
        self.no_color
    }

    /// A highlighted list row. Without colour a background can't mark it, so
    /// it is reversed instead.
    pub fn selection(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().bg(self.c(Slot::SelectionBg))
        }
    }

    /// The active tab of a tabbed dialog header.
    pub fn heading(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::REVERSED).bold()
        } else {
            Style::default()
                .fg(self.c(Slot::HeadingFg))
                .bg(self.c(Slot::HeadingBg))
                .bold()
        }
    }

    /// A profile's hashed colour, tuned to stay readable on the base's
    /// background (bright on dark, deep on light).
    pub fn profile_color(&self, name: &str) -> Color {
        match (self.no_color, self.base) {
            (true, _) => Color::Reset,
            (false, ThemeBase::Dark) => hash_profile_color(name),
            (false, ThemeBase::Light) => hashed_color(name, LIGHT_PROFILE_VALUE),
        }
    }

    /// Every colour, for render-cache fingerprinting.
    pub fn palette(&self) -> &[Color; Slot::COUNT] {
        &self.palette
    }

    pub fn user_colors(self, profile_color: Color) -> RoleColors {
        self.role(profile_color, Slot::MessageBg)
    }

    pub fn user_input_colors(self, profile_color: Color) -> RoleColors {
        self.role(profile_color, Slot::InputBg)
    }

    pub fn assistant_colors(&self) -> RoleColors {
        self.role(self.c(Slot::Assistant), Slot::MessageBg)
    }

    pub fn reasoning_colors(&self) -> RoleColors {
        self.role(self.c(Slot::Reasoning), Slot::MessageBg)
    }

    pub fn tool_req_colors(&self) -> RoleColors {
        self.role(self.c(Slot::ToolRequest), Slot::MessageBg)
    }

    pub fn tool_out_colors(&self) -> RoleColors {
        self.role(self.c(Slot::ToolOutput), Slot::MessageBg)
    }

    pub fn error_colors(&self) -> RoleColors {
        self.role(self.c(Slot::Error), Slot::MessageBg)
    }

    pub fn sidebar_colors(&self) -> RoleColors {
        self.role(self.c(Slot::SidebarFg), Slot::SidebarBg)
    }

    fn role(&self, fg: Color, bg: Slot) -> RoleColors {
        RoleColors { fg, bg: self.c(bg) }
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

/// HSV value of a hashed profile colour on `light`: at 40 every hue keeps
/// WCAG ≥ 4.5:1 on the light backgrounds (tested over all hues).
pub(crate) const LIGHT_PROFILE_VALUE: u8 = 40;

/// The dark theme's hashed profile colour.
pub(crate) fn hash_profile_color(name: &str) -> Color {
    hashed_color(name, 90)
}

fn hashed_color(name: &str, value: u8) -> Color {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(name, &mut hasher);
    let hash = hasher.finish();

    let hue = (hash % 360) as u8;
    hsv_to_rgb(hue, 70, value)
}

pub(crate) fn hsv_to_rgb(h: u8, s: u8, v: u8) -> Color {
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
mod tests;
