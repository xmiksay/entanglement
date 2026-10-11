//! The built-in palettes behind [`crate::config::theme::Slot`] (#735).
//!
//! Both tables are exhaustive `match`es, so a new slot fails to compile until
//! every base defines it. `dark` is the pre-#735 colours exactly; `light`
//! targets a light terminal background — every foreground slot keeps a WCAG
//! contrast ratio of at least 4.5:1 against `message.bg` and `input.bg`
//! (asserted below).

use ratatui::style::Color;

use crate::config::theme::{Slot, ThemeBase, ThemeColor};

/// The base's colour for `slot`.
pub(crate) fn base_color(base: ThemeBase, slot: Slot) -> Color {
    match base {
        ThemeBase::Dark => dark(slot),
        ThemeBase::Light => light(slot),
    }
}

fn dark(slot: Slot) -> Color {
    use Slot::*;
    match slot {
        MessageBg => Color::Rgb(30, 30, 35),
        InputBg | SidebarBg => Color::Rgb(26, 26, 33),
        SidebarFg | Assistant | ToolRequest | Accent | StatusBranch => Color::Cyan,
        Reasoning | ToolOutput | Secondary => Color::Gray,
        Text => Color::White,
        Success | DiffAdd => Color::Green,
        Warning | Approval | MarkdownCode | StatusAttention => Color::Yellow,
        Error | DiffRemove => Color::Red,
        Special | StatusWorktree => Color::Magenta,
        Info => Color::Blue,
        Muted | SelectionBg | DiffContext => Color::DarkGray,
        HeadingFg => Color::Black,
        HeadingBg => Color::Cyan,
        StatusRepo | StatusPath | StatusText | StatusDim => Color::Reset,
    }
}

const TEAL: Color = Color::Rgb(0, 110, 140);
const AMBER: Color = Color::Rgb(140, 85, 0);
const CRIMSON: Color = Color::Rgb(180, 30, 30);
const FOREST: Color = Color::Rgb(30, 110, 40);
const PLUM: Color = Color::Rgb(140, 50, 150);
const SLATE: Color = Color::Rgb(90, 90, 100);

fn light(slot: Slot) -> Color {
    use Slot::*;
    match slot {
        MessageBg => Color::Rgb(238, 238, 242),
        InputBg | SidebarBg => Color::Rgb(228, 228, 235),
        SidebarFg | Assistant | ToolRequest | Accent | StatusBranch | HeadingBg => TEAL,
        Reasoning | Secondary => SLATE,
        ToolOutput => Color::Rgb(70, 70, 80),
        Text => Color::Rgb(20, 20, 25),
        Success | DiffAdd => FOREST,
        Warning | Approval | StatusAttention => AMBER,
        MarkdownCode => Color::Rgb(150, 60, 0),
        Error | DiffRemove => CRIMSON,
        Special | StatusWorktree => PLUM,
        Info => Color::Rgb(30, 80, 170),
        Muted | DiffContext => Color::Rgb(100, 100, 110),
        SelectionBg => Color::Rgb(200, 212, 230),
        HeadingFg => Color::Rgb(255, 255, 255),
        StatusRepo | StatusPath | StatusText | StatusDim => Color::Reset,
    }
}

pub(crate) fn to_ratatui(c: ThemeColor) -> Color {
    const ANSI: [Color; 16] = [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ];
    match c {
        ThemeColor::Reset => Color::Reset,
        ThemeColor::Ansi(i) => ANSI.get(i as usize).copied().unwrap_or(Color::Reset),
        ThemeColor::Indexed(i) => Color::Indexed(i),
        ThemeColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(c: Color) -> f64 {
        let Color::Rgb(r, g, b) = c else {
            panic!("light palette colours are explicit RGB: {c:?}");
        };
        let lin = |v: u8| {
            let v = v as f64 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
    }

    fn contrast(a: Color, b: Color) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn light_foregrounds_are_readable_on_light_backgrounds() {
        let backgrounds = [Slot::MessageBg, Slot::InputBg, Slot::SidebarBg];
        for slot in Slot::ALL {
            let fg = light(*slot);
            let is_bg = slot.name().ends_with(".bg");
            if is_bg || fg == Color::Reset || *slot == Slot::HeadingFg {
                continue;
            }
            for bg in backgrounds {
                let ratio = contrast(fg, light(bg));
                assert!(ratio >= 4.5, "{} on {}: {ratio:.2}", slot.name(), bg.name());
            }
        }
        let heading = contrast(light(Slot::HeadingFg), light(Slot::HeadingBg));
        assert!(heading >= 4.5, "heading: {heading:.2}");
        // A selected row keeps the terminal's (dark) default text readable.
        assert!(contrast(light(Slot::SelectionBg), Color::Rgb(0, 0, 0)) >= 7.0);
    }

    /// Hashed profile colours (mode badge, user bar, session list) hold the
    /// same 4.5:1 bar on `light` for every hue the hash can produce (the hue
    /// is a `u8`).
    #[test]
    fn light_profile_colours_are_readable_for_every_hue() {
        use crate::tui::theme::{hsv_to_rgb, LIGHT_PROFILE_VALUE};
        for hue in 0..=u8::MAX {
            let fg = hsv_to_rgb(hue, 70, LIGHT_PROFILE_VALUE);
            for bg in [Slot::MessageBg, Slot::InputBg, Slot::SidebarBg] {
                let ratio = contrast(fg, light(bg));
                assert!(ratio >= 4.5, "hue {hue} on {}: {ratio:.2}", bg.name());
            }
        }
    }

    #[test]
    fn every_slot_has_a_light_value_distinct_from_dark_where_dark_is_unreadable() {
        // Dark's named colours (`White` input text, `Yellow` warnings) vanish on
        // a light background, so light never reuses an ANSI name.
        for slot in Slot::ALL {
            assert!(
                matches!(light(*slot), Color::Rgb(..) | Color::Reset),
                "{}",
                slot.name()
            );
        }
    }

    #[test]
    fn config_colours_map_onto_ratatui() {
        assert_eq!(to_ratatui(ThemeColor::Ansi(6)), Color::Cyan);
        assert_eq!(to_ratatui(ThemeColor::Ansi(8)), Color::DarkGray);
        assert_eq!(to_ratatui(ThemeColor::Ansi(15)), Color::White);
        assert_eq!(to_ratatui(ThemeColor::Indexed(208)), Color::Indexed(208));
        assert_eq!(to_ratatui(ThemeColor::Rgb(1, 2, 3)), Color::Rgb(1, 2, 3));
        assert_eq!(to_ratatui(ThemeColor::Reset), Color::Reset);
    }
}
