//! `/cd [--session|--always] [<dir>]` (#764, ADR-0224): change the active
//! session's working directory through `InMsg::SetWorkdir`. The engine
//! validates it (the runtime's workdir resolver) and acks with
//! `WorkdirChanged`, which the status line shows; a refusal — not a
//! directory, outside the root without approval — lands as an `Error`.
//!
//! A directory outside the project root needs the escape-root approval
//! (ADR-0109). There is no tool call to park an approval card on here, so
//! the user's answer is the flag on the command itself: the first plain
//! `/cd` outside the root is refused with the hint, and `--session` (this
//! run) or `--always` (persisted) resends it approved. Once granted, later
//! `/cd`s to the same directory need no flag.

use std::path::Path;

use entanglement_core::{ApprovalScope, Holly, InMsg};

use crate::tui::app::App;

/// Parse and send `/cd`; a usage error renders as a status line.
pub(crate) async fn send_cd(app: &mut App, holly: &Holly, text: &str) {
    let home = dirs::home_dir();
    match parse_cd_args(text, app.root(), home.as_deref()) {
        Ok((dir, approve)) => {
            let _ = holly
                .send(InMsg::SetWorkdir {
                    session: app.active_session_id().clone(),
                    dir,
                    approve,
                })
                .await;
        }
        Err(message) => app.record_status("cd", message),
    }
}

/// `/cd …` → `(dir, approve)`. No directory means the project root; a
/// leading `~` expands to `home`. Relative directories go to the engine as
/// typed — they resolve against the session's current workdir there.
fn parse_cd_args(
    text: &str,
    root: &Path,
    home: Option<&Path>,
) -> Result<(String, Option<ApprovalScope>), String> {
    let rest = text.trim().strip_prefix("/cd").unwrap_or(text).trim();
    let mut approve = None;
    let mut words = Vec::new();
    for word in rest.split_whitespace() {
        match word {
            "--session" => approve = Some(ApprovalScope::Session),
            "--always" => approve = Some(ApprovalScope::Always),
            flag if flag.starts_with("--") => {
                return Err(format!(
                    "usage: /cd [--session|--always] [<dir>] — unknown flag `{flag}`"
                ))
            }
            word => words.push(word),
        }
    }
    let dir = words.join(" ");
    let dir = match (dir.as_str(), home) {
        ("", _) => root.to_string_lossy().into_owned(),
        ("~", Some(home)) => home.to_string_lossy().into_owned(),
        (d, Some(home)) if d.starts_with("~/") => home.join(&d[2..]).to_string_lossy().into_owned(),
        _ => dir,
    };
    Ok((dir, approve))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<(String, Option<ApprovalScope>), String> {
        parse_cd_args(text, Path::new("/p"), Some(Path::new("/home/u")))
    }

    #[test]
    fn a_relative_directory_goes_as_typed() {
        assert_eq!(parse("/cd src/a b"), Ok(("src/a b".into(), None)));
        assert_eq!(parse("/cd .."), Ok(("..".into(), None)));
    }

    #[test]
    fn bare_cd_returns_to_the_project_root() {
        assert_eq!(parse("/cd"), Ok(("/p".into(), None)));
        assert_eq!(parse("/cd   "), Ok(("/p".into(), None)));
    }

    #[test]
    fn the_approval_flags_set_the_scope() {
        assert_eq!(
            parse("/cd --session /x"),
            Ok(("/x".into(), Some(ApprovalScope::Session)))
        );
        assert_eq!(
            parse("/cd /x --always"),
            Ok(("/x".into(), Some(ApprovalScope::Always)))
        );
        assert!(parse("/cd --once /x").is_err());
    }

    #[test]
    fn a_tilde_expands_to_home() {
        assert_eq!(parse("/cd ~"), Ok(("/home/u".into(), None)));
        assert_eq!(parse("/cd ~/w"), Ok(("/home/u/w".into(), None)));
        assert_eq!(
            parse_cd_args("/cd ~/w", Path::new("/p"), None),
            Ok(("~/w".into(), None))
        );
    }
}
