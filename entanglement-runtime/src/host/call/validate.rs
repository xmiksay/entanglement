//! Pre-spawn shape check for the shell-less `call` tool.
//!
//! `call` execs `command` + `args` verbatim with no `sh -c`, so a model that
//! writes a whole shell command line into `command` (e.g. `gh pr view 446
//! --json …`) makes `spawn()` fail with an opaque ENOENT on a "binary" named
//! after the entire line — the failure that stranded the PR-#446 review, where
//! the model looped the same malformed call until the turn died because the
//! `spawning \`…\`` error carried no hint. This guard turns that silent loop
//! into one self-correcting error that names the fix (split into `command` +
//! `args`, or reach for `bash` when a shell is actually needed).

use anyhow::{bail, Result};

/// Shell metacharacters `call` never interprets. Their presence in `command`
/// means a shell line was written, not an argv — always an error here.
const SHELL_METACHARS: &[char] = &['|', '&', ';', '<', '>', '$', '`', '(', ')', '\n'];

/// Reject a `command` that is really a shell command line, before `spawn()`.
///
/// Two independent signals: any [`SHELL_METACHARS`] char (a pipe/redirect/`&&`
/// only a shell honours), or multiple whitespace-separated tokens with an empty
/// `args` (the whole line stuffed into `command`). The whitespace signal is
/// suppressed when `command` looks like a path (`/`, `./`, `../`) so a genuine
/// spaced executable path — `/opt/My App/tool` — is never a false positive; a
/// well-formed call passing that same path *with* `args` is untouched regardless.
pub(super) fn check_no_shell(command: &str, args: &[String]) -> Result<()> {
    let trimmed = command.trim();
    let looks_like_path =
        trimmed.starts_with('/') || trimmed.starts_with("./") || trimmed.starts_with("../");
    let multiple_tokens = trimmed.split_whitespace().count() > 1;
    let has_metachar = trimmed.contains(|c| SHELL_METACHARS.contains(&c));
    let stuffed = multiple_tokens && args.is_empty() && !looks_like_path;
    if !has_metachar && !stuffed {
        return Ok(());
    }

    // Only offer a concrete split when the fix really is "move the tail into
    // args"; a metachar line needs `bash`, not a naive whitespace split.
    let suggestion = match trimmed.split_whitespace().collect::<Vec<_>>().split_first() {
        Some((prog, rest)) if !rest.is_empty() && !has_metachar => {
            let quoted = rest
                .iter()
                .map(|a| format!("\"{a}\""))
                .collect::<Vec<_>>()
                .join(", ");
            format!(" e.g. {{\"command\": \"{prog}\", \"args\": [{quoted}]}}")
        }
        _ => String::new(),
    };
    bail!(
        "`call` runs one executable with NO shell: put the program name in \
         `command` and each argument in `args` (pipes, redirects `>`, `&&`, \
         `$VAR`, and globs need the `bash` tool instead). Received command = \
         `{command}`.{suggestion}"
    );
}

#[cfg(test)]
mod tests {
    use super::check_no_shell;

    fn err(command: &str, args: &[&str]) -> String {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        check_no_shell(command, &args)
            .expect_err("expected a shape error")
            .to_string()
    }

    #[test]
    fn whole_command_line_stuffed_into_command_is_rejected_with_a_split() {
        let msg = err("gh pr view 446 --json title,body", &[]);
        assert!(msg.contains("NO shell"), "got: {msg}");
        // The suggestion turns the stuffed line into a proper argv.
        assert!(
            msg.contains(
                r#"{"command": "gh", "args": ["pr", "view", "446", "--json", "title,body"]}"#
            ),
            "got: {msg}"
        );
    }

    #[test]
    fn multi_token_command_without_args_is_rejected() {
        assert!(err("git remote -v", &[]).contains("`command`"));
    }

    #[test]
    fn shell_metacharacters_are_rejected_and_steer_to_bash() {
        let msg = err("gh pr diff 446 > /tmp/pr446.diff", &[]);
        assert!(msg.contains("`bash` tool"), "got: {msg}");
        // A metachar line gets no naive whitespace-split suggestion.
        assert!(!msg.contains("e.g."), "got: {msg}");
    }

    #[test]
    fn bare_executable_passes() {
        check_no_shell("pwd", &[]).unwrap();
    }

    #[test]
    fn executable_with_verbatim_args_passes() {
        let args = vec!["%s".to_string(), "hello world".to_string()];
        check_no_shell("printf", &args).unwrap();
    }

    #[test]
    fn spaced_executable_path_is_not_a_false_positive() {
        // A real path may contain spaces; the path exemption keeps it valid
        // both with and without args.
        check_no_shell("/opt/My App/tool", &[]).unwrap();
        check_no_shell("/opt/My App/tool", &["--flag".to_string()]).unwrap();
    }
}
