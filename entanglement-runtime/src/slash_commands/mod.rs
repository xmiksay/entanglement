//! User-defined slash commands (#733, ADR-0221): markdown prompt templates a
//! user invokes as `/name args…`, expanded head-side into an ordinary
//! `Prompt`. Nothing here reaches the engine or the protocol — a command is
//! sugar for typing its expanded text.
//!
//! # Layers
//!
//! Discovered like skills (ADR-0074) through [`crate::layers`], later wins on
//! a name collision; there are no embedded built-ins:
//!
//! 1. **user** — `~/.claude/commands`, `${config_dir}/opencode/command(s)`
//!    (lenient), then `${config_dir}/entanglement/commands` (strict); or
//!    exactly `ENTANGLEMENT_COMMANDS_DIR` when set.
//! 2. **project** — `.claude/commands`, `.agents/commands`,
//!    `.opencode/command(s)` (lenient), then `.entanglement/commands`
//!    (strict, highest).
//!
//! Every `*.md` under a dir is a command named by its relative path, with
//! subdirectories as `:` namespaces (`frontend/review.md` → `/frontend:review`).
//!
//! # Built-in names win
//!
//! A command whose name equals a built-in TUI command ([`RESERVED_NAMES`]) is
//! dropped with a load warning — in every head, so `skutter run "/help"`
//! means the same thing it would in the TUI.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};

use anyhow::{Context, Result};

use crate::layers::{Layer, Strictness, UserLayer};

mod discovery;
mod expand;
mod parse;

pub use discovery::COMMANDS_DIR_ENV;
pub use expand::{parse_invocation, quote_argv, render, split_args};

/// Built-in TUI slash commands. A user command can never shadow one; the TUI
/// asserts this list matches its own roster in a unit test, so the two can't
/// drift.
pub const RESERVED_NAMES: &[&str] = &[
    "help",
    "new",
    "resume",
    "exit",
    "agent",
    "model",
    "mode",
    "key",
    "plan",
    "tasks",
    "inspect",
    "editor",
    "export",
    "compact",
    "cost",
    "set",
    "show",
    "mcp",
    "allow",
    "enable",
    "disable",
    "tools",
    "pause",
    "continue",
    "stop",
    "name",
    "aux-model",
    "undo",
    "rewind",
    "fork",
    "init",
];

/// One resolved user command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserCommand {
    /// Invocation name without the `/` (`review`, `frontend:review`).
    pub name: String,
    /// Frontmatter `description`, else the body's first non-empty line.
    pub description: String,
    /// Frontmatter `argument-hint`, shown next to the name (`<file>`).
    pub argument_hint: Option<String>,
    /// The prompt template ([`render`] expands it).
    pub body: String,
    pub layer: Layer,
    /// The file the winning definition came from.
    pub source: String,
}

impl UserCommand {
    /// The template expanded for `args`.
    pub fn expand(&self, args: &str) -> String {
        render(&self.body, args)
    }

    /// `/name <hint>` — the one-line usage the TUI whisper shows.
    pub fn usage(&self) -> String {
        match &self.argument_hint {
            Some(hint) => format!("/{} {hint}", self.name),
            None => format!("/{}", self.name),
        }
    }
}

/// The winning commands, keyed (and so iterated) by name.
#[derive(Debug, Clone, Default)]
pub struct CommandRegistry {
    commands: BTreeMap<String, Arc<UserCommand>>,
}

/// The runtime-held, live-reloadable registry handle the definitions watcher
/// swaps on a change (#329 pattern) and the heads read on every use.
pub type LiveCommands = Arc<RwLock<Arc<CommandRegistry>>>;

impl CommandRegistry {
    pub fn get(&self, name: &str) -> Option<&Arc<UserCommand>> {
        self.commands.get(name)
    }

    /// Every command, name-sorted.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<UserCommand>> {
        self.commands.values()
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Expand `/name args…` when `name` is a known command: the command and
    /// its rendered prompt. `None` for anything else (plain text, a built-in,
    /// an unknown `/word`), which the caller sends as typed.
    pub fn expand(&self, input: &str) -> Option<(Arc<UserCommand>, String)> {
        let (name, args) = parse_invocation(input)?;
        let command = self.get(name)?;
        Some((command.clone(), command.expand(args)))
    }
}

/// The prompt a head should send for `input`: its expansion when it invokes a
/// user command, otherwise `input` unchanged. The `run` head's entry point —
/// `skutter run "/review foo"` expands exactly as the TUI's Enter does.
pub fn expand_or_literal(live: &LiveCommands, input: String) -> String {
    let registry = live.read().unwrap_or_else(PoisonError::into_inner).clone();
    match registry.expand(&input) {
        Some((_, prompt)) => prompt,
        None => input,
    }
}

/// [`expand_or_literal`] for a CLI argv (`skutter run /review "a b"`): the
/// tokens are rejoined with [`quote_argv`] so the shell's quoting survives
/// into `$1`..`$9`; a non-command prompt is sent as the plain space-joined
/// text, exactly as before (#798).
pub fn expand_argv_or_literal(live: &LiveCommands, argv: &[String]) -> String {
    let registry = live.read().unwrap_or_else(PoisonError::into_inner).clone();
    match registry.expand(&quote_argv(argv)) {
        Some((_, prompt)) => prompt,
        None => argv.join(" "),
    }
}

/// One command name with full provenance, for `skutter inspect commands`.
#[derive(Debug, Clone)]
pub struct CommandResolution {
    /// The highest-layer definition.
    pub command: UserCommand,
    /// Lower-layer definitions it overrode, `(layer, source)`, lowest first.
    pub shadowed: Vec<(Layer, String)>,
    /// The name is a built-in TUI command, so this definition is never used.
    pub builtin_clash: bool,
}

/// Resolve every command for `root` with provenance, name-sorted. A malformed
/// file in a native dir aborts; a malformed foreign file is warned and skipped
/// (ADR-0074).
pub fn resolve_registry(root: &Path) -> Result<Vec<CommandResolution>> {
    resolve_registry_in(root, &UserLayer::FromEnv)
}

/// [`resolve_registry`] with an explicit user layer (#773).
pub fn resolve_registry_in(root: &Path, user: &UserLayer) -> Result<Vec<CommandResolution>> {
    resolve(discovery::discover(root, user)?)
}

fn resolve(raws: Vec<discovery::RawCommand>) -> Result<Vec<CommandResolution>> {
    let mut by_name: BTreeMap<String, CommandResolution> = BTreeMap::new();
    for raw in raws {
        let parsed = match parse::parse(&raw.name, &raw.content, raw.strictness) {
            Ok(p) => p,
            Err(e) if raw.strictness == Strictness::Lenient => {
                tracing::warn!(
                    source = %raw.source,
                    error = %format!("{e:#}"),
                    "skipping malformed foreign command definition",
                );
                continue;
            }
            Err(e) => return Err(e).with_context(|| format!("parsing command {}", raw.source)),
        };
        if !parsed.warn_ignored.is_empty() {
            tracing::warn!(
                command = %raw.name,
                source = %raw.source,
                keys = ?parsed.warn_ignored,
                "command frontmatter keys are ignored: a command runs in the current \
                 session with its agent, model and permission mode; remove them",
            );
        }
        let command = UserCommand {
            name: raw.name.clone(),
            description: parsed.description,
            argument_hint: parsed.argument_hint,
            body: parsed.body,
            layer: raw.layer,
            source: raw.source,
        };
        match by_name.get_mut(&raw.name) {
            Some(prior) => {
                tracing::debug!(
                    command = %raw.name,
                    replaces = %prior.command.source,
                    source = %command.source,
                    "command definition overrides a lower layer",
                );
                let old = std::mem::replace(&mut prior.command, command);
                prior.shadowed.push((old.layer, old.source));
            }
            None => {
                let builtin_clash = RESERVED_NAMES.contains(&raw.name.as_str());
                by_name.insert(
                    raw.name,
                    CommandResolution {
                        command,
                        shadowed: Vec::new(),
                        builtin_clash,
                    },
                );
            }
        }
    }
    Ok(by_name.into_values().collect())
}

/// Load the registry heads use: [`resolve_registry`] minus built-in clashes,
/// each warned once.
pub fn load_registry(root: &Path) -> Result<CommandRegistry> {
    Ok(registry_from(resolve_registry(root)?))
}

fn registry_from(resolved: Vec<CommandResolution>) -> CommandRegistry {
    resolved
        .into_iter()
        .filter(|r| {
            if r.builtin_clash {
                tracing::warn!(
                    command = %r.command.name,
                    source = %r.command.source,
                    "user command shadows a built-in slash command and is ignored — rename the file",
                );
            }
            !r.builtin_clash
        })
        .map(|r| r.command)
        .collect()
}

/// An in-memory registry (an embedder's own commands, or a test's). Later
/// entries win on a name; built-in names are *not* filtered here — that is
/// [`load_registry`]'s job for discovered files.
impl FromIterator<UserCommand> for CommandRegistry {
    fn from_iter<I: IntoIterator<Item = UserCommand>>(iter: I) -> Self {
        let commands = iter
            .into_iter()
            .map(|c| (c.name.clone(), Arc::new(c)))
            .collect();
        CommandRegistry { commands }
    }
}

#[cfg(test)]
mod tests;
