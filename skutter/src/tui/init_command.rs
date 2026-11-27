//! `/init` (#732): ask the model to write an `AGENTS.md` brief for the
//! project. Head-side sugar for typing a prompt, like a user command
//! (ADR-0221): the head builds a built-in prompt template plus a **bounded**
//! repository snapshot and sends it as an ordinary `InMsg::Prompt`. The model
//! then writes the file with its normal `write`/`edit` tools, so the session's
//! permission mode grades the write like any other — there is no bypass.
//!
//! Bounds: the snapshot reuses the `@file` completion index (the `glob` walk,
//! capped at its own scan and result limits, `.gitignore`-aware), never a
//! fresh unbounded walk — skutter often runs from a huge workspace root. The
//! prompt then lists at most [`MAX_LISTED`] paths.
//!
//! An existing `AGENTS.md` is never overwritten on a bare `/init`: the user
//! confirms with `/init --update`, which asks the model to improve it instead.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use entanglement_core::{Holly, InMsg};

use crate::tui::app::App;
use crate::tui::mention::FileIndex;

pub(crate) const AGENTS_FILE: &str = "AGENTS.md";
/// Sample paths listed in the prompt.
const MAX_LISTED: usize = 200;
const MAX_TOP_LEVEL: usize = 60;
const MAX_EXTENSIONS: usize = 12;
/// Files worth pointing the model at first, when present.
const NOTABLE: &[&str] = &[
    "README.md",
    "AGENTS.md",
    "CLAUDE.md",
    ".claude/CLAUDE.md",
    "CONTRIBUTING.md",
    "Makefile",
    "justfile",
    "Cargo.toml",
    "rust-toolchain.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "CMakeLists.txt",
    "Gemfile",
    "Dockerfile",
    "docker-compose.yml",
    "docs/architecture.md",
];
const USAGE: &str = "usage: /init [--update] [extra instructions…]";

/// What `/init` resolved to.
#[derive(Debug, PartialEq)]
pub(crate) enum InitPlan {
    /// Say this and send nothing.
    Refuse(String),
    /// Send this prompt as an ordinary turn.
    Prompt(String),
}

/// Resolve `/init`'s raw input `text` (leading `/init` included) against
/// `root` and the indexed `files` (root-relative, `/`-separated).
pub(crate) fn plan(root: &Path, files: &[String], text: &str) -> InitPlan {
    let args = text.trim().strip_prefix("/init").unwrap_or(text).trim();
    let (update, extra) = match args.split_once(char::is_whitespace) {
        _ if args == "--update" => (true, ""),
        Some(("--update", rest)) => (true, rest.trim()),
        _ if args.starts_with("--") => return InitPlan::Refuse(USAGE.into()),
        _ => (false, args),
    };
    let exists = root.join(AGENTS_FILE).exists();
    if exists && !update {
        return InitPlan::Refuse(format!(
            "{AGENTS_FILE} already exists in {} — not overwriting it. \
             Run `/init --update` to have the model review and improve it instead.",
            root.display()
        ));
    }
    InitPlan::Prompt(prompt(files, exists, extra))
}

/// The built-in prompt template plus the repository snapshot.
fn prompt(files: &[String], update: bool, extra: &str) -> String {
    let mut out = String::new();
    if update {
        out.push_str(
            "Improve the existing `AGENTS.md` at the project root — the brief coding \
             agents read first. Read it, keep what is still accurate, fix what is \
             stale and fill the gaps. Apply the changes with `edit` (or `write` for a \
             full rewrite). Do not touch any other file.\n\n",
        );
    } else {
        out.push_str(
            "Analyse this repository and write an `AGENTS.md` brief at the project \
             root — the file coding agents read first. Create it with the `write` \
             tool. Write only that one file.\n\n",
        );
    }
    out.push_str(
        "Ground every statement in files you actually read: open the manifests, the \
         README, the Makefile / CI config and a few representative sources first. \
         Never invent a command. Keep it concise (well under ~150 lines) and cover:\n\
         - what the project is, in two or three sentences;\n\
         - how to build, test, lint, format and run it (exact commands);\n\
         - the layout: main directories / crates / packages and what lives where;\n\
         - conventions a contributor must follow (style, errors, tests, commits);\n\
         - gotchas: non-obvious setup, generated files, things not to edit.\n\n",
    );
    out.push_str(&snapshot(files));
    if !extra.is_empty() {
        let _ = write!(out, "\nAdditional instructions: {extra}\n");
    }
    out
}

/// A bounded summary of the indexed tree: notable files, top-level entries,
/// a file-type histogram and the shallowest [`MAX_LISTED`] paths.
fn snapshot(files: &[String]) -> String {
    let mut out = format!(
        "Repository snapshot ({} indexed files; the index is bounded and skips \
         ignored paths, so a large tree is only partly shown):\n",
        files.len()
    );
    let notable: Vec<&str> = NOTABLE
        .iter()
        .copied()
        .filter(|n| {
            files
                .iter()
                .any(|f| f == n || f.starts_with(&format!("{n}/")))
        })
        .collect();
    if !notable.is_empty() {
        let _ = writeln!(out, "Notable files: {}", notable.join(", "));
    }

    let mut top: Vec<String> = Vec::new();
    for f in files {
        let entry = match f.split_once('/') {
            Some((dir, _)) => format!("{dir}/"),
            None => f.clone(),
        };
        if !top.contains(&entry) {
            top.push(entry);
        }
    }
    top.sort();
    let shown = top.len().min(MAX_TOP_LEVEL);
    let _ = write!(out, "Top level: {}", top[..shown].join(", "));
    if top.len() > shown {
        let _ = write!(out, ", … ({} more)", top.len() - shown);
    }
    out.push('\n');

    let mut by_ext: BTreeMap<&str, usize> = BTreeMap::new();
    for f in files {
        let name = f.rsplit('/').next().unwrap_or(f);
        if let Some((stem, ext)) = name.rsplit_once('.') {
            if !stem.is_empty() {
                *by_ext.entry(ext).or_default() += 1;
            }
        }
    }
    let mut exts: Vec<(&str, usize)> = by_ext.into_iter().collect();
    exts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let exts: Vec<String> = exts
        .iter()
        .take(MAX_EXTENSIONS)
        .map(|(e, n)| format!(".{e} {n}"))
        .collect();
    if !exts.is_empty() {
        let _ = writeln!(out, "File types: {}", exts.join(", "));
    }

    let mut sample: Vec<&String> = files.iter().collect();
    sample.sort_by_key(|f| (f.matches('/').count(), f.as_str()));
    sample.truncate(MAX_LISTED);
    let _ = writeln!(
        out,
        "Paths (shallowest first, {} of {}):",
        sample.len(),
        files.len()
    );
    for f in sample {
        let _ = writeln!(out, "  {f}");
    }
    out
}

/// `/init` from the input line (`text`) or the palette (`"/init"`).
pub(crate) async fn send_init(app: &mut App, holly: &Holly, text: &str) {
    let root = app.root().to_path_buf();
    let mut files = app.mention().index().files().to_vec();
    if files.is_empty() {
        // The background `@file` index hasn't landed yet: the same bounded
        // walk, off the async worker.
        let walk_root = root.clone();
        files = tokio::task::spawn_blocking(move || FileIndex::build(&walk_root))
            .await
            .map(|index| index.files().to_vec())
            .unwrap_or_default();
    }
    match plan(&root, &files, text) {
        InitPlan::Refuse(message) => app.record_notice("/init", message),
        InitPlan::Prompt(prompt) => {
            app.record_notice(
                "/init",
                format!(
                    "asking the model to write {AGENTS_FILE}; the write is graded \
                     by the session's permission mode like any other"
                ),
            );
            app.record_user_message(prompt.clone());
            let session = app.active_session_id().clone();
            if let Err(e) = holly.send(InMsg::prompt(session, prompt)).await {
                tracing::debug!("failed to send the /init prompt: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn writes_a_new_brief_through_the_write_tool() {
        let dir = tempfile::tempdir().unwrap();
        let listed = files(&["Cargo.toml", "Makefile", "src/main.rs", "src/lib.rs"]);
        let InitPlan::Prompt(p) = plan(dir.path(), &listed, "/init") else {
            panic!("expected a prompt");
        };
        assert!(p.contains("write an `AGENTS.md` brief"), "{p}");
        assert!(p.contains("`write` tool"), "{p}");
        assert!(p.contains("Notable files: Makefile, Cargo.toml"), "{p}");
        assert!(p.contains("Top level: Cargo.toml, Makefile, src/"), "{p}");
        assert!(p.contains(".rs 2"), "{p}");
    }

    #[test]
    fn refuses_to_overwrite_an_existing_brief() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(AGENTS_FILE), "# brief\n").unwrap();
        let InitPlan::Refuse(msg) = plan(dir.path(), &[], "/init") else {
            panic!("an existing AGENTS.md must not be overwritten");
        };
        assert!(msg.contains("/init --update"), "{msg}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(AGENTS_FILE)).unwrap(),
            "# brief\n"
        );
    }

    #[test]
    fn update_confirms_and_asks_for_an_improvement() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(AGENTS_FILE), "# brief\n").unwrap();
        let InitPlan::Prompt(p) = plan(dir.path(), &[], "/init --update focus on tests") else {
            panic!("--update must proceed");
        };
        assert!(p.starts_with("Improve the existing `AGENTS.md`"), "{p}");
        assert!(p.contains("Additional instructions: focus on tests"), "{p}");
    }

    #[test]
    fn an_unknown_flag_shows_usage() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            plan(dir.path(), &[], "/init --force"),
            InitPlan::Refuse(USAGE.into())
        );
    }

    /// A big tree stays bounded twice over: the shared index walk caps what is
    /// indexed, and the prompt caps what is listed.
    #[test]
    fn a_big_tree_yields_a_bounded_prompt() {
        let dir = tempfile::tempdir().unwrap();
        for d in 0..40 {
            let sub = dir.path().join(format!("pkg{d:02}/src"));
            std::fs::create_dir_all(&sub).unwrap();
            for f in 0..40 {
                std::fs::write(sub.join(format!("m{f:02}.rs")), "").unwrap();
            }
        }
        let index = FileIndex::build(dir.path());
        assert!(index.files().len() <= 1000, "the walk caps the index");
        let InitPlan::Prompt(p) = plan(dir.path(), index.files(), "/init") else {
            panic!("expected a prompt");
        };
        let listed = p.lines().filter(|l| l.starts_with("  pkg")).count();
        assert_eq!(listed, MAX_LISTED);
        assert!(p.len() < 32 * 1024, "prompt is {} bytes", p.len());
    }
}
