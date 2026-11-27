use super::*;
use crate::config::theme::{ThemeColor, ThemeConfig};

fn themed(yaml: &str, no_color: bool) -> Theme {
    let cfg: ThemeConfig = serde_yaml::from_str(yaml).unwrap();
    Theme::resolve(&cfg.resolve(), no_color)
}

#[test]
fn test_darken_rgb() {
    let darkened = darken(Color::Rgb(100, 150, 200), 0.15);
    match darkened {
        Color::Rgb(r, g, b) => assert!(r < 100 && g < 150 && b < 200),
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
    assert_eq!(theme.c(Slot::Assistant), Color::Cyan);
    assert_eq!(theme.c(Slot::MessageBg), Color::Rgb(30, 30, 35));
    assert_eq!(theme.base(), ThemeBase::Dark);
}

#[test]
fn test_theme_colors() {
    let assistant = Theme::default().assistant_colors();
    assert_eq!(assistant.fg, Color::Cyan);
    assert_eq!(assistant.bg, Color::Rgb(30, 30, 35));
}

#[test]
fn test_decorate_line() {
    let theme = Theme::default();
    let decorated = theme.decorate(Line::from("test"), theme.assistant_colors(), 20);
    assert!(decorated.spans.len() >= 3);
    assert_eq!(decorated.spans[0].content.as_ref(), "▌");
    assert_eq!(decorated.spans[1].content.as_ref(), " ");
    assert!(decorated.spans.iter().any(|s| s.content.as_ref() == "test"));
}

/// The dark default is the pre-#735 hardcoded colours, slot for slot.
#[test]
fn dark_default_matches_the_pre_735_colours() {
    let t = Theme::default();
    let expect = [
        (Slot::MessageBg, Color::Rgb(30, 30, 35)),
        (Slot::InputBg, Color::Rgb(26, 26, 33)),
        (Slot::SidebarBg, Color::Rgb(26, 26, 33)),
        (Slot::SidebarFg, Color::Cyan),
        (Slot::Assistant, Color::Cyan),
        (Slot::Reasoning, Color::Gray),
        (Slot::ToolRequest, Color::Cyan),
        (Slot::ToolOutput, Color::Gray),
        (Slot::Text, Color::White),
        (Slot::Accent, Color::Cyan),
        (Slot::Success, Color::Green),
        (Slot::Warning, Color::Yellow),
        (Slot::Error, Color::Red),
        (Slot::Special, Color::Magenta),
        (Slot::Info, Color::Blue),
        (Slot::Muted, Color::DarkGray),
        (Slot::Secondary, Color::Gray),
        (Slot::SelectionBg, Color::DarkGray),
        (Slot::HeadingFg, Color::Black),
        (Slot::HeadingBg, Color::Cyan),
        (Slot::Approval, Color::Yellow),
        (Slot::MarkdownCode, Color::Yellow),
        (Slot::DiffAdd, Color::Green),
        (Slot::DiffRemove, Color::Red),
        (Slot::DiffContext, Color::DarkGray),
        (Slot::StatusBranch, Color::Cyan),
        (Slot::StatusWorktree, Color::Magenta),
        (Slot::StatusAttention, Color::Yellow),
    ];
    for (slot, c) in expect {
        assert_eq!(t.c(slot), c, "{}", slot.name());
    }
    assert_eq!(
        t.status,
        StatusTheme {
            repo: Style::default().bold(),
            path: Style::default(),
            branch: Style::default().fg(Color::Cyan),
            worktree: Style::default().fg(Color::Magenta),
            text: Style::default(),
            dim: Style::default().dim(),
            attention: Style::default().fg(Color::Yellow).bold(),
        }
    );
    assert_eq!(t.selection(), Style::default().bg(Color::DarkGray));
    assert_eq!(
        t.heading(),
        Style::default().fg(Color::Black).bg(Color::Cyan).bold()
    );
    assert_eq!(t.profile_color("build"), hash_profile_color("build"));
}

#[test]
fn light_defines_every_slot_and_differs_from_dark() {
    let (dark, light) = (Theme::default(), themed("light", false));
    assert_eq!(light.base(), ThemeBase::Light);
    for slot in Slot::ALL {
        let c = light.c(*slot);
        if c != Color::Reset {
            assert_ne!(c, dark.c(*slot), "{} reuses the dark colour", slot.name());
        }
    }
    assert_ne!(light.profile_color("build"), dark.profile_color("build"));
}

#[test]
fn a_palette_override_replaces_only_its_slot() {
    let t = themed(
        "{base: light, slots: {status.branch: cyan, diff.add: '#2e7d32', muted: 244, bogus: red}}",
        false,
    );
    let light = themed("light", false);
    assert_eq!(t.c(Slot::StatusBranch), Color::Cyan);
    assert_eq!(t.status.branch, Style::default().fg(Color::Cyan));
    assert_eq!(t.c(Slot::DiffAdd), Color::Rgb(0x2e, 0x7d, 0x32));
    assert_eq!(t.c(Slot::Muted), Color::Indexed(244));
    assert_eq!(t.c(Slot::Accent), light.c(Slot::Accent), "untouched slot");
}

#[test]
fn a_bad_colour_falls_back_to_the_base() {
    let cfg: ThemeConfig = serde_yaml::from_str("{slots: {accent: '#12'}}").unwrap();
    let resolved = cfg.resolve();
    assert_eq!(resolved.warnings.len(), 1);
    assert_eq!(
        Theme::resolve(&resolved, false).c(Slot::Accent),
        Color::Cyan
    );
}

#[test]
fn no_color_resets_every_slot_but_keeps_emphasis() {
    let t = themed("{base: light, slots: {accent: red}}", true);
    for slot in Slot::ALL {
        assert_eq!(t.c(*slot), Color::Reset, "{}", slot.name());
    }
    assert!(t.no_color());
    assert_eq!(t.status.repo, Style::default().bold());
    assert_eq!(t.status.dim, Style::default().dim());
    assert_eq!(t.status.attention, Style::default().bold());
    assert!(t.selection().add_modifier.contains(Modifier::REVERSED));
    assert!(t.heading().add_modifier.contains(Modifier::BOLD));
    assert_eq!(t.profile_color("build"), Color::Reset);
}

#[test]
fn overrides_apply_in_config_order_onto_the_base() {
    let resolved = ResolvedTheme {
        base: ThemeBase::Dark,
        overrides: vec![(Slot::Error, ThemeColor::Ansi(9))],
        warnings: vec![],
    };
    assert_eq!(
        Theme::resolve(&resolved, false).c(Slot::Error),
        Color::LightRed
    );
}
