//! Command-file parsing (#733): optional `---` YAML frontmatter + a markdown
//! body that is the prompt template.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::layers::Strictness;

/// The frontmatter keys a command file may carry. Every key is optional — a
/// plain markdown file with no frontmatter at all is a valid command (Claude
/// Code and opencode both allow it). Unknown keys land in `extra`: a native
/// (strict) file rejects them loudly, a foreign (lenient) file ignores them
/// (ADR-0074).
#[derive(Debug, Default, Deserialize)]
struct CommandFrontmatter {
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "argument-hint", alias = "argument_hint")]
    argument_hint: Option<String>,
    /// Parsed but ignored (see [`IGNORED_KEYS`]).
    #[serde(default)]
    agent: Option<serde_yaml::Value>,
    #[serde(default)]
    model: Option<serde_yaml::Value>,
    #[serde(default, rename = "allowed-tools", alias = "allowed_tools")]
    allowed_tools: Option<serde_yaml::Value>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_yaml::Value>,
}

/// A parsed command, before layer/provenance bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Parsed {
    pub(super) description: String,
    pub(super) argument_hint: Option<String>,
    pub(super) body: String,
    /// Ignored keys the caller should warn about — native files only.
    /// `agent`, `model` and `allowed-tools` are accepted but do nothing: a
    /// command runs in the *current* session, whose agent is fixed for its
    /// whole life (ADR-0207 §9) and whose model only `/model` switches, and a
    /// command never narrows or widens authority — the permission mode is the
    /// sole control (ADR-0194's additive-only rule, applied to commands).
    /// Foreign files routinely carry these for their own tool, so warning
    /// there would be noise on every load; a native file opted in on purpose.
    pub(super) warn_ignored: Vec<&'static str>,
}

/// Parse one command file. `name` only feeds validation and warnings.
pub(super) fn parse(name: &str, content: &str, strictness: Strictness) -> Result<Parsed> {
    if name.is_empty() || name.contains(char::is_whitespace) || name.split(':').any(str::is_empty) {
        bail!("invalid command name `{name}`: the file path must not contain whitespace");
    }
    let opens_with_fence = content.lines().next().map(str::trim) == Some("---");
    let (frontmatter, body) = if opens_with_fence {
        crate::frontmatter::split(content)?
    } else {
        (String::new(), content.to_string())
    };
    let fm: CommandFrontmatter = if frontmatter.trim().is_empty() {
        CommandFrontmatter::default()
    } else {
        serde_yaml::from_str(&frontmatter).context("invalid command frontmatter")?
    };
    if strictness == Strictness::Strict {
        if let Some(key) = fm.extra.keys().next() {
            bail!("unknown command frontmatter key `{key}`");
        }
    }
    let warn_ignored = if strictness == Strictness::Strict {
        [
            ("agent", fm.agent.is_some()),
            ("model", fm.model.is_some()),
            ("allowed-tools", fm.allowed_tools.is_some()),
        ]
        .into_iter()
        .filter_map(|(key, present)| present.then_some(key))
        .collect()
    } else {
        Vec::new()
    };
    let body = body.trim().to_string();
    let description = fm
        .description
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .or_else(|| first_line(&body))
        .unwrap_or_default();
    Ok(Parsed {
        description,
        argument_hint: fm.argument_hint.filter(|h| !h.trim().is_empty()),
        body,
        warn_ignored,
    })
}

/// The body's first non-empty line, the fallback description (Claude Code's
/// own default for a command without one).
fn first_line(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_native_frontmatter() {
        let p = parse(
            "review",
            "---\ndescription: Review a file\nargument-hint: <file>\n---\nReview $1\n",
            Strictness::Strict,
        )
        .unwrap();
        assert_eq!(
            p,
            Parsed {
                description: "Review a file".into(),
                argument_hint: Some("<file>".into()),
                body: "Review $1".into(),
                warn_ignored: Vec::new(),
            }
        );
    }

    #[test]
    fn underscore_hint_alias_is_accepted() {
        let p = parse(
            "x",
            "---\nargument_hint: '[n]'\n---\nbody",
            Strictness::Strict,
        )
        .unwrap();
        assert_eq!(p.argument_hint.as_deref(), Some("[n]"));
    }

    #[test]
    fn no_frontmatter_uses_the_first_body_line_as_description() {
        let p = parse("x", "\n  Explain the code.\nMore.", Strictness::Strict).unwrap();
        assert_eq!(p.description, "Explain the code.");
        assert_eq!(p.body, "Explain the code.\nMore.");
        assert_eq!(p.argument_hint, None);
    }

    #[test]
    fn strict_rejects_unknown_keys_lenient_ignores_them() {
        let content = "---\ndescription: d\ncolor: red\n---\nbody";
        let err = parse("x", content, Strictness::Strict).unwrap_err();
        assert!(err.to_string().contains("color"), "{err}");
        let p = parse("x", content, Strictness::Lenient).unwrap();
        assert_eq!(p.description, "d");
    }

    #[test]
    fn ignored_keys_parse_in_both_strictnesses() {
        let content = "---\nagent: plan\nmodel: x/y\nallowed-tools: Bash(git:*)\n---\nbody";
        for s in [Strictness::Strict, Strictness::Lenient] {
            let p = parse("x", content, s).unwrap();
            assert_eq!(p.body, "body");
        }
        // The underscore spelling of `allowed-tools` too (the skills spelling).
        parse(
            "x",
            "---\nallowed_tools: [bash]\n---\nb",
            Strictness::Strict,
        )
        .unwrap();
    }

    /// A native file carrying an ignored key is flagged for a load warning
    /// naming exactly those keys; a foreign file carrying the same keys (its
    /// own tool's) is not.
    #[test]
    fn ignored_keys_are_flagged_for_native_files_only() {
        let content = "---\nagent: plan\nmodel: x/y\n---\nbody";
        let native = parse("x", content, Strictness::Strict).unwrap();
        assert_eq!(native.warn_ignored, vec!["agent", "model"]);
        let foreign = parse("x", content, Strictness::Lenient).unwrap();
        assert!(foreign.warn_ignored.is_empty());
        let plain = parse("y", "---\ndescription: d\n---\nb", Strictness::Strict).unwrap();
        assert!(plain.warn_ignored.is_empty());
    }

    #[test]
    fn invalid_names_are_refused() {
        for bad in ["", "has space", "a::b", ":a"] {
            assert!(parse(bad, "body", Strictness::Lenient).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn malformed_yaml_is_an_error() {
        assert!(parse("x", "---\ndescription: [\n---\nb", Strictness::Lenient).is_err());
        assert!(parse("x", "---\ndescription: d\n", Strictness::Lenient).is_err());
    }
}
