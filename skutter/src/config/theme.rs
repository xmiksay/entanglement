//! `tui.theme` — the TUI colour palette (#735).
//!
//! Either a built-in base name (`dark`, the default and the pre-#735 colours
//! exactly, or `light`) or an object overriding individual named slots on top
//! of a base: `{ base: light, slots: { "diff.add": "#2e7d32" } }`. Every
//! colour the TUI draws comes from one [`Slot`]; the TUI owns the per-base
//! values (`tui::theme::palette`), this module owns the names and the colour
//! grammar so the config loader (and `skutter inspect config`) can warn about a
//! bad entry without a TUI build.
//!
//! Lenient by design, unlike the rest of `config.yml`: an unknown base, an
//! unknown slot or an unparseable colour is one warning each and falls back to
//! the base's value — a typo in a colour must never stop the TUI starting.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_yaml::Value;

macro_rules! slots {
    ($($variant:ident => $name:literal),* $(,)?) => {
        /// One named colour slot of the TUI palette.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Slot { $($variant),* }

        impl Slot {
            /// Every slot, in declaration order (also the palette index order).
            pub const ALL: &'static [Slot] = &[$(Slot::$variant),*];

            /// The slot's config name.
            pub fn name(self) -> &'static str {
                match self { $(Slot::$variant => $name),* }
            }
        }
    };
}

slots! {
    MessageBg => "message.bg",
    InputBg => "input.bg",
    SidebarBg => "sidebar.bg",
    SidebarFg => "sidebar.fg",
    Assistant => "assistant",
    Reasoning => "reasoning",
    ToolRequest => "tool.request",
    ToolOutput => "tool.output",
    Text => "text",
    Accent => "accent",
    Success => "success",
    Warning => "warning",
    Error => "error",
    Special => "special",
    Info => "info",
    Muted => "muted",
    Secondary => "secondary",
    SelectionBg => "selection.bg",
    HeadingFg => "heading.fg",
    HeadingBg => "heading.bg",
    Approval => "approval",
    MarkdownCode => "markdown.code",
    DiffAdd => "diff.add",
    DiffRemove => "diff.remove",
    DiffContext => "diff.context",
    StatusRepo => "status.repo",
    StatusPath => "status.path",
    StatusBranch => "status.branch",
    StatusWorktree => "status.worktree",
    StatusText => "status.text",
    StatusDim => "status.dim",
    StatusAttention => "status.attention",
}

impl Slot {
    pub const COUNT: usize = Slot::ALL.len();

    pub fn from_name(name: &str) -> Option<Slot> {
        Slot::ALL.iter().copied().find(|s| s.name() == name)
    }
}

/// A built-in palette a theme starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeBase {
    #[default]
    Dark,
    Light,
}

impl ThemeBase {
    pub fn name(self) -> &'static str {
        match self {
            ThemeBase::Dark => "dark",
            ThemeBase::Light => "light",
        }
    }
}

/// A parsed colour, independent of any terminal library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeColor {
    /// The terminal's own default (`default` / `reset`).
    Reset,
    /// One of the 16 ANSI colours, `0..=15` in the conventional order
    /// (black, red, green, yellow, blue, magenta, cyan, gray, dark_gray,
    /// light_red … light_cyan, white).
    Ansi(u8),
    /// A 256-colour palette index.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

const ANSI_NAMES: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "gray",
    "darkgray",
    "lightred",
    "lightgreen",
    "lightyellow",
    "lightblue",
    "lightmagenta",
    "lightcyan",
    "white",
];

/// Parse a colour: a named ANSI colour (`cyan`, `dark_gray`, `light-red`;
/// `grey` spellings accepted), `default`/`reset`, `#rrggbb`, or a 256-colour
/// index `0..=255`.
pub fn parse_color(spec: &str) -> Result<ThemeColor, String> {
    let s = spec.trim();
    if let Some(hex) = s.strip_prefix('#') {
        let byte = |i: usize| {
            hex.get(i..i + 2)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        };
        return match (hex.len(), byte(0), byte(2), byte(4)) {
            (6, Some(r), Some(g), Some(b)) => Ok(ThemeColor::Rgb(r, g, b)),
            _ => Err(format!("`{spec}` is not a `#rrggbb` colour")),
        };
    }
    if let Ok(n) = s.parse::<u16>() {
        return u8::try_from(n)
            .map(ThemeColor::Indexed)
            .map_err(|_| format!("`{spec}` is outside the 256-colour range 0..=255"));
    }
    let norm: String = s
        .chars()
        .filter(|c| !matches!(c, '_' | '-' | ' '))
        .collect::<String>()
        .to_ascii_lowercase()
        .replace("grey", "gray");
    if norm == "default" || norm == "reset" {
        return Ok(ThemeColor::Reset);
    }
    ANSI_NAMES
        .iter()
        .position(|n| *n == norm)
        .map(|i| ThemeColor::Ansi(i as u8))
        .ok_or_else(|| {
            format!(
                "`{spec}` is not a colour (a named ANSI colour, `default`, `#rrggbb` or 0..=255)"
            )
        })
}

/// Raw `tui.theme` value: a base name, or a base plus slot overrides.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "Value")]
pub struct ThemeConfig {
    pub base: String,
    /// Slot name → colour spec, kept raw so a bad entry warns instead of
    /// failing the whole config.
    pub slots: BTreeMap<String, String>,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            base: ThemeBase::Dark.name().to_string(),
            slots: BTreeMap::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PaletteObject {
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    slots: BTreeMap<String, Value>,
}

impl TryFrom<Value> for ThemeConfig {
    type Error = String;

    fn try_from(v: Value) -> Result<Self, String> {
        match v {
            Value::String(base) => Ok(Self {
                base,
                ..Self::default()
            }),
            Value::Mapping(_) => {
                let p: PaletteObject = serde_yaml::from_value(v).map_err(|e| e.to_string())?;
                let mut slots = BTreeMap::new();
                for (name, value) in p.slots {
                    // A bare YAML number (`21`) is a 256-colour index.
                    let spec = match value {
                        Value::String(s) => s,
                        Value::Number(n) => n.to_string(),
                        other => format!("{other:?}"),
                    };
                    slots.insert(name, spec);
                }
                Ok(Self {
                    base: p.base.unwrap_or_else(|| ThemeBase::Dark.name().to_string()),
                    slots,
                })
            }
            _ => Err("tui.theme must be a theme name (`dark`/`light`) or \
                      `{ base: <name>, slots: { <slot>: <colour> } }`"
                .to_string()),
        }
    }
}

/// [`ThemeConfig`] checked: the base, the valid overrides, and one warning
/// per rejected entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTheme {
    pub base: ThemeBase,
    pub overrides: Vec<(Slot, ThemeColor)>,
    pub warnings: Vec<String>,
}

impl ThemeConfig {
    pub fn resolve(&self) -> ResolvedTheme {
        let mut warnings = Vec::new();
        let base = match self.base.as_str() {
            "dark" => ThemeBase::Dark,
            "light" => ThemeBase::Light,
            other => {
                warnings.push(format!(
                    "tui.theme: unknown theme `{other}` — using `dark` (built-in: dark, light)"
                ));
                ThemeBase::Dark
            }
        };
        let mut overrides = Vec::new();
        for (name, spec) in &self.slots {
            let Some(slot) = Slot::from_name(name) else {
                warnings.push(format!(
                    "tui.theme.slots: unknown slot `{name}` is ignored (known: {})",
                    Slot::ALL
                        .iter()
                        .map(|s| s.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                continue;
            };
            match parse_color(spec) {
                Ok(c) => overrides.push((slot, c)),
                Err(e) => warnings.push(format!(
                    "tui.theme.slots.{name}: {e} — keeping the `{}` theme's colour",
                    base.name()
                )),
            }
        }
        ResolvedTheme {
            base,
            overrides,
            warnings,
        }
    }

    pub fn warnings(&self) -> Vec<String> {
        self.resolve().warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<ThemeConfig, serde_yaml::Error> {
        serde_yaml::from_str(yaml)
    }

    #[test]
    fn colours_parse_in_every_supported_spelling() {
        assert_eq!(parse_color("cyan"), Ok(ThemeColor::Ansi(6)));
        assert_eq!(parse_color("Dark_Grey"), Ok(ThemeColor::Ansi(8)));
        assert_eq!(parse_color("light-red"), Ok(ThemeColor::Ansi(9)));
        assert_eq!(parse_color("white"), Ok(ThemeColor::Ansi(15)));
        assert_eq!(parse_color("default"), Ok(ThemeColor::Reset));
        assert_eq!(
            parse_color("#2e7D32"),
            Ok(ThemeColor::Rgb(0x2e, 0x7d, 0x32))
        );
        assert_eq!(parse_color("208"), Ok(ThemeColor::Indexed(208)));
        for bad in ["#12345", "#gg0000", "256", "chartreuse", ""] {
            assert!(parse_color(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn slot_names_are_unique_and_round_trip() {
        for s in Slot::ALL {
            assert_eq!(Slot::from_name(s.name()), Some(*s));
        }
        let mut names: Vec<_> = Slot::ALL.iter().map(|s| s.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Slot::COUNT);
    }

    #[test]
    fn a_bare_name_or_a_palette_object_parses() {
        assert_eq!(parse("dark").unwrap(), ThemeConfig::default());
        let c = parse("light").unwrap();
        assert_eq!(c.resolve().base, ThemeBase::Light);
        let c =
            parse("{base: light, slots: {status.branch: cyan, diff.add: '#2e7d32', muted: 244}}")
                .unwrap();
        let r = c.resolve();
        assert_eq!(r.base, ThemeBase::Light);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        assert!(r
            .overrides
            .contains(&(Slot::DiffAdd, ThemeColor::Rgb(0x2e, 0x7d, 0x32))));
        assert!(r
            .overrides
            .contains(&(Slot::Muted, ThemeColor::Indexed(244))));
        assert_eq!(
            parse("{slots: {}}").unwrap().resolve().base,
            ThemeBase::Dark
        );
    }

    #[test]
    fn bad_entries_warn_once_each_and_are_dropped() {
        let c =
            parse("{base: solar, slots: {nope: red, accent: '#zzzzzz', success: green}}").unwrap();
        let r = c.resolve();
        assert_eq!(r.base, ThemeBase::Dark);
        assert_eq!(r.overrides, vec![(Slot::Success, ThemeColor::Ansi(2))]);
        assert_eq!(r.warnings.len(), 3, "{:?}", r.warnings);
        assert!(r.warnings.iter().any(|w| w.contains("`solar`")));
        assert!(r.warnings.iter().any(|w| w.contains("`nope`")));
        assert!(r
            .warnings
            .iter()
            .any(|w| w.contains("tui.theme.slots.accent")));
    }

    #[test]
    fn a_typo_in_the_object_keys_or_a_wrong_shape_is_loud() {
        assert!(parse("{bsae: light}").is_err());
        assert!(parse("[dark]").is_err());
    }
}
