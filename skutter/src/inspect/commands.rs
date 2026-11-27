//! `skutter inspect commands [name]` (#733): the user slash commands —
//! which file each `/name` resolved to, what it overrode, and whether a
//! built-in TUI command hides it. Engine-free, like every inspect view.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use crate::slash_commands::{self, CommandResolution};

/// The table (no `name`) or one command's detail.
pub fn inspect_commands(cwd: &Path, name: Option<&str>) -> Result<()> {
    let resolved = slash_commands::resolve_registry(cwd).context("resolving slash commands")?;
    match name {
        Some(name) => {
            let name = name.trim_start_matches('/');
            let entry = resolved
                .iter()
                .find(|r| r.command.name == name)
                .with_context(|| format!("unknown command `/{name}` (no file in any layer)"))?;
            print!("{}", render_command_detail(entry));
        }
        None => print!("{}", render_command_table(&resolved)),
    }
    Ok(())
}

/// One row per command name: usage, winning layer, source file, description.
/// A built-in clash stays listed (marked) so its author can see why `/name`
/// does something else. Shared with the TUI `/inspect` Commands tab.
pub(super) fn render_command_table(resolved: &[CommandResolution]) -> String {
    if resolved.is_empty() {
        return "no slash command definitions found\n".to_string();
    }
    let rows: Vec<Vec<String>> = resolved
        .iter()
        .map(|r| {
            let usage = r.command.usage();
            vec![
                if r.builtin_clash {
                    format!("{usage} (hidden by built-in)")
                } else {
                    usage
                },
                r.command.layer.label().to_string(),
                r.command.source.clone(),
                r.command.description.clone(),
            ]
        })
        .collect();
    super::render_table(&["COMMAND", "LAYER", "SOURCE", "DESCRIPTION"], &rows)
}

/// Identity, provenance and the raw (unexpanded) template for one command.
pub(super) fn render_command_detail(entry: &CommandResolution) -> String {
    let cmd = &entry.command;
    let mut out = String::new();
    let _ = writeln!(out, "usage:       {}", cmd.usage());
    let _ = writeln!(out, "description: {}", cmd.description);
    let _ = writeln!(out, "layer:       {}", cmd.layer.label());
    let _ = writeln!(out, "source:      {}", cmd.source);
    if entry.shadowed.is_empty() {
        let _ = writeln!(out, "overrides:   (none — no lower-layer definition)");
    } else {
        let _ = writeln!(out, "overrides:");
        for (layer, source) in &entry.shadowed {
            let _ = writeln!(out, "  - {} ({source})", layer.label());
        }
    }
    if entry.builtin_clash {
        let _ = writeln!(
            out,
            "\nnote: `/{}` is a built-in command, which always wins — this file is \
             never used; rename it.",
            cmd.name
        );
    }
    let _ = writeln!(out, "\n─── template ($ARGUMENTS, $1..$9) ───\n");
    let _ = writeln!(out, "{}", cmd.body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::Layer;
    use crate::slash_commands::UserCommand;

    fn resolution(name: &str, clash: bool) -> CommandResolution {
        CommandResolution {
            command: UserCommand {
                name: name.into(),
                description: "Review a file".into(),
                argument_hint: Some("<file>".into()),
                body: "Review $1".into(),
                layer: Layer::Project,
                source: "/p/.entanglement/commands/review.md".into(),
            },
            shadowed: vec![(Layer::User, "/u/review.md".into())],
            builtin_clash: clash,
        }
    }

    #[test]
    fn table_lists_usage_layer_source_and_description() {
        let out = render_command_table(&[resolution("review", false), resolution("help", true)]);
        assert!(out.starts_with("COMMAND"), "{out}");
        assert!(out.contains("/review <file>"), "{out}");
        assert!(out.contains("project"), "{out}");
        assert!(out.contains("/p/.entanglement/commands/review.md"), "{out}");
        assert!(out.contains("Review a file"), "{out}");
        assert!(out.contains("/help <file> (hidden by built-in)"), "{out}");
        assert_eq!(
            render_command_table(&[]),
            "no slash command definitions found\n"
        );
    }

    #[test]
    fn detail_shows_provenance_and_raw_template() {
        let out = render_command_detail(&resolution("review", false));
        assert!(out.contains("overrides:\n  - user (/u/review.md)"), "{out}");
        assert!(out.contains("Review $1"), "{out}");
        assert!(!out.contains("built-in command"), "{out}");
        let clash = render_command_detail(&resolution("help", true));
        assert!(clash.contains("built-in command"), "{clash}");
    }
}
