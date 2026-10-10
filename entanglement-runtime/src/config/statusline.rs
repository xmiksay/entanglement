//! `tui.statusline` — the TUI's always-visible project/path line (#762).
//!
//! Two templates share one placeholder grammar: `template` renders
//! left-aligned, `right` right-aligned on the same row. A placeholder is
//! `{name}` with `name` from [`PLACEHOLDERS`]; anything else — an unknown
//! name, a lone brace — is literal text. An unknown `{name}` is rendered
//! literally *and* reported once as a config warning, so a typo is visible
//! both on screen and in `skutter inspect config`.
//!
//! The tokenizer lives here (lean library) rather than in the TUI so the
//! config loader can warn about unknown placeholders without a TUI build.
//! Part B's `command:` runner (and its `timeout_ms`) will join this struct;
//! they are deliberately absent until then — `deny_unknown_fields` keeps a
//! premature key loud instead of silently inert.

use serde::Deserialize;

/// Every placeholder the TUI fills. Order is irrelevant; the renderer maps
/// each name to its value.
pub const PLACEHOLDERS: &[&str] = &[
    "repo",
    "cwd",
    "cwd_rel",
    "branch",
    "worktree",
    "session",
    "agent",
    "mode",
    "model",
    "cost",
    "context_pct",
    "action",
    "tasks",
];

const DEFAULT_TEMPLATE: &str = "{repo} › {cwd_rel}  ⎇ {branch} {worktree}";
const DEFAULT_RIGHT: &str = "{session}";

/// Resolved `tui.statusline` settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatuslineConfig {
    /// The left-aligned line.
    pub template: String,
    /// The right-aligned line, sharing the row with `template`.
    pub right: String,
}

impl Default for StatuslineConfig {
    fn default() -> Self {
        Self {
            template: DEFAULT_TEMPLATE.to_string(),
            right: DEFAULT_RIGHT.to_string(),
        }
    }
}

impl StatuslineConfig {
    /// One warning per distinct unknown placeholder across both templates.
    pub fn warnings(&self) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        let mut warnings = Vec::new();
        for (key, tpl) in [("template", &self.template), ("right", &self.right)] {
            for name in tokenize(tpl).1 {
                if seen.contains(&name) {
                    continue;
                }
                warnings.push(format!(
                    "tui.statusline.{key}: unknown placeholder `{{{name}}}` is rendered \
                     literally (known: {})",
                    PLACEHOLDERS.join(", ")
                ));
                seen.push(name);
            }
        }
        warnings
    }
}

/// One piece of a parsed template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Lit(String),
    Var(&'static str),
}

/// Split `template` into literal runs and known placeholders (adjacent
/// literals merged), plus the names of any unknown `{name}` — which stay in
/// the literal text verbatim.
pub fn tokenize(template: &str) -> (Vec<Token>, Vec<String>) {
    let mut tokens: Vec<Token> = Vec::new();
    let mut unknown = Vec::new();
    let mut lit = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        lit.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let name_len = after
            .find(|c: char| !(c.is_ascii_lowercase() || c == '_'))
            .unwrap_or(after.len());
        let closed = name_len > 0 && after[name_len..].starts_with('}');
        if !closed {
            lit.push('{');
            rest = after;
            continue;
        }
        let name = &after[..name_len];
        match PLACEHOLDERS.iter().find(|p| **p == name) {
            Some(known) => {
                if !lit.is_empty() {
                    tokens.push(Token::Lit(std::mem::take(&mut lit)));
                }
                tokens.push(Token::Var(known));
            }
            None => {
                lit.push_str(&rest[open..open + name_len + 2]);
                unknown.push(name.to_string());
            }
        }
        rest = &after[name_len + 1..];
    }
    lit.push_str(rest);
    if !lit.is_empty() {
        tokens.push(Token::Lit(lit));
    }
    (tokens, unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(s: &str) -> Token {
        Token::Lit(s.to_string())
    }

    #[test]
    fn defaults_are_the_approved_layout() {
        let c = StatuslineConfig::default();
        assert_eq!(c.template, "{repo} › {cwd_rel}  ⎇ {branch} {worktree}");
        assert_eq!(c.right, "{session}");
        assert!(c.warnings().is_empty());
    }

    #[test]
    fn tokenize_splits_literals_and_known_placeholders() {
        let (t, unknown) = tokenize("{repo} › {cwd_rel}!");
        assert_eq!(
            t,
            vec![
                Token::Var("repo"),
                lit(" › "),
                Token::Var("cwd_rel"),
                lit("!")
            ]
        );
        assert!(unknown.is_empty());
    }

    #[test]
    fn unknown_and_malformed_braces_stay_literal() {
        let (t, unknown) = tokenize("a {nope} {Repo} { x {repo");
        assert_eq!(t, vec![lit("a {nope} {Repo} { x {repo")]);
        assert_eq!(unknown, vec!["nope".to_string()]);
        let (t, _) = tokenize("{}{repo}");
        assert_eq!(t, vec![lit("{}"), Token::Var("repo")]);
    }

    #[test]
    fn each_unknown_placeholder_warns_once() {
        let c = StatuslineConfig {
            template: "{foo} {foo} {repo}".into(),
            right: "{foo} {bar}".into(),
        };
        let w = c.warnings();
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].contains("`{foo}`") && w[0].contains("tui.statusline.template"));
        assert!(w[1].contains("`{bar}`") && w[1].contains("tui.statusline.right"));
    }

    #[test]
    fn fields_are_optional_and_strict() {
        let c: StatuslineConfig = serde_yaml::from_str("right: ''\n").unwrap();
        assert_eq!(c.template, StatuslineConfig::default().template);
        assert!(c.right.is_empty());
        assert!(serde_yaml::from_str::<StatuslineConfig>("command: x\n").is_err());
    }
}
