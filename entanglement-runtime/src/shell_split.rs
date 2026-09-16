//! Conservative, quote-aware splitter for POSIX-ish bash command lines
//! (ADR-0197): breaks a compound command into its top-level segments so each
//! can be graded against an argument-scoped permission rule independently,
//! instead of the whole raw string being graded as one opaque blob —
//! `entanglement-runtime/src/permission_bash.rs` is the grading side that
//! consumes this.
//!
//! Deliberately narrow: anything the splitter cannot fully account for —
//! output redirection, command/process substitution, a heredoc, subshell
//! grouping, an unmatched quote — comes back [`SplitOutcome::Opaque`] rather
//! than a best-effort guess, so a caller can fail closed instead of silently
//! mis-grading a smuggled side effect. Every "fail closed" choice below is
//! deliberate, not an oversight — see the inline WHY on each.

/// The result of [`split`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitOutcome {
    /// Top-level segments, in order, trimmed and non-empty. A simple command
    /// with no top-level operator yields exactly one segment equal to the
    /// trimmed input.
    Segments(Vec<String>),
    /// The command contains a construct the splitter refuses to reason
    /// about (see the module doc) — callers must treat this as "can't
    /// tell", never as "no segments".
    Opaque,
}

/// Split `command` on top-level `&&`, `||`, `;`, `|`, `&` (background), and
/// newlines, honoring single/double quotes and backslash escapes. See the
/// module doc for exactly which constructs force [`SplitOutcome::Opaque`].
pub fn split(command: &str) -> SplitOutcome {
    let chars: Vec<char> = command.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut segments = Vec::new();
    let mut current = String::new();

    while i < n {
        let c = chars[i];

        if in_single {
            // Single quotes: everything is literal, including backslash,
            // until the closing quote.
            current.push(c);
            if c == '\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }

        // Backslash escapes the next character verbatim, in both unquoted
        // and double-quoted context. Real bash's double-quote escape set is
        // narrower (only `$` `` ` `` `"` `\` and newline), but treating every
        // escape as literal only ever makes MORE text opaque-to-us as plain
        // content — it can never turn a literal character into an operator
        // we'd otherwise miss, so it stays on the fail-closed side. A
        // trailing backslash with nothing to escape is ambiguous — fail
        // closed rather than guess what the shell would do with it.
        if c == '\\' {
            if i + 1 >= n {
                return SplitOutcome::Opaque;
            }
            current.push(c);
            current.push(chars[i + 1]);
            i += 2;
            continue;
        }

        if in_double {
            match c {
                '"' => {
                    in_double = false;
                    current.push(c);
                    i += 1;
                }
                // Command substitution still expands inside double quotes.
                '$' if peek(&chars, i + 1) == Some('(') => return SplitOutcome::Opaque,
                '`' => return SplitOutcome::Opaque,
                _ => {
                    current.push(c);
                    i += 1;
                }
            }
            continue;
        }

        match c {
            '\'' => {
                in_single = true;
                current.push(c);
                i += 1;
            }
            '"' => {
                in_double = true;
                current.push(c);
                i += 1;
            }
            '`' => return SplitOutcome::Opaque,
            '$' if peek(&chars, i + 1) == Some('(') => return SplitOutcome::Opaque,
            // Process substitution `<(...)`/`>(...)` is command execution in
            // disguise, not plain redirection — never safe to fold into a
            // graded segment.
            '<' if peek(&chars, i + 1) == Some('(') => return SplitOutcome::Opaque,
            '>' if peek(&chars, i + 1) == Some('(') => return SplitOutcome::Opaque,
            // Heredoc (`<<`/`<<-`): its body is literal text that spans the
            // following newlines, which this splitter would otherwise
            // misread as segment separators — opaque rather than
            // mis-splitting the body.
            '<' if peek(&chars, i + 1) == Some('<') => return SplitOutcome::Opaque,
            // Output redirection (`>`/`>>`) can append arbitrary bytes
            // anywhere a curated read-only rule allowed a command to run —
            // never safe to fold into a segment. Bare stdin `<` is left as
            // plain segment text below; it only reads, matching the design
            // doc's explicit carve-out.
            '>' => return SplitOutcome::Opaque,
            // Subshell grouping / arithmetic `((...))` is not implemented —
            // any bare paren is opaque rather than guessed at.
            '(' | ')' => return SplitOutcome::Opaque,
            '&' if peek(&chars, i + 1) == Some('&') => {
                push_segment(&mut segments, &mut current);
                i += 2;
            }
            '|' if peek(&chars, i + 1) == Some('|') => {
                push_segment(&mut segments, &mut current);
                i += 2;
            }
            '|' | ';' | '&' | '\n' => {
                push_segment(&mut segments, &mut current);
                i += 1;
            }
            _ => {
                current.push(c);
                i += 1;
            }
        }
    }

    if in_single || in_double {
        // Unmatched quote — the splitter cannot know where the command
        // (or a shell metacharacter inside the unterminated quote) really
        // ends.
        return SplitOutcome::Opaque;
    }
    push_segment(&mut segments, &mut current);
    SplitOutcome::Segments(segments)
}

fn peek(chars: &[char], i: usize) -> Option<char> {
    chars.get(i).copied()
}

fn push_segment(segments: &mut Vec<String>, current: &mut String) {
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }
    current.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(command: &str) -> Vec<String> {
        match split(command) {
            SplitOutcome::Segments(s) => s,
            SplitOutcome::Opaque => panic!("expected Segments for {command:?}, got Opaque"),
        }
    }

    #[test]
    fn simple_command_is_one_segment_equal_to_input() {
        assert_eq!(segs("find ."), vec!["find ."]);
    }

    #[test]
    fn splits_on_and_and() {
        assert_eq!(
            segs("find . && rm -rf /tmp/x"),
            vec!["find .", "rm -rf /tmp/x"]
        );
    }

    #[test]
    fn splits_on_pipe_chain() {
        assert_eq!(
            segs("find . | grep x | wc -l"),
            vec!["find .", "grep x", "wc -l"]
        );
    }

    #[test]
    fn splits_on_semicolon() {
        assert_eq!(segs("git status; find ."), vec!["git status", "find ."]);
    }

    #[test]
    fn splits_on_newline() {
        assert_eq!(segs("git status\nfind ."), vec!["git status", "find ."]);
    }

    #[test]
    fn splits_on_background_ampersand() {
        assert_eq!(segs("sleep 5 & find ."), vec!["sleep 5", "find ."]);
    }

    #[test]
    fn splits_on_or() {
        assert_eq!(segs("find . || echo no"), vec!["find .", "echo no"]);
    }

    #[test]
    fn double_quoted_operator_stays_in_one_segment() {
        assert_eq!(segs(r#"grep "a && b" f"#), vec![r#"grep "a && b" f"#]);
    }

    #[test]
    fn single_quoted_operator_stays_in_one_segment() {
        assert_eq!(segs("grep 'a | b' f"), vec!["grep 'a | b' f"]);
    }

    #[test]
    fn backslash_escaped_operator_stays_in_one_segment() {
        assert_eq!(segs(r"echo a \&\& b"), vec![r"echo a \&\& b"]);
    }

    #[test]
    fn stdin_redirect_is_not_opaque() {
        assert_eq!(segs("wc -l < file.txt"), vec!["wc -l < file.txt"]);
    }

    #[test]
    fn output_redirect_is_opaque() {
        assert_eq!(split("find . > out.txt"), SplitOutcome::Opaque);
    }

    #[test]
    fn append_redirect_is_opaque() {
        assert_eq!(split("echo hi >> out.txt"), SplitOutcome::Opaque);
    }

    #[test]
    fn command_substitution_is_opaque() {
        assert_eq!(split("echo $(rm -rf /)"), SplitOutcome::Opaque);
    }

    #[test]
    fn backtick_substitution_is_opaque() {
        assert_eq!(split("echo `rm -rf /`"), SplitOutcome::Opaque);
    }

    #[test]
    fn command_substitution_inside_double_quotes_is_opaque() {
        assert_eq!(split(r#"echo "$(rm -rf /)""#), SplitOutcome::Opaque);
    }

    #[test]
    fn process_substitution_is_opaque() {
        assert_eq!(split("diff <(ls a) <(ls b)"), SplitOutcome::Opaque);
    }

    #[test]
    fn heredoc_is_opaque() {
        assert_eq!(split("cat <<EOF\nhi\nEOF"), SplitOutcome::Opaque);
    }

    #[test]
    fn subshell_grouping_is_opaque() {
        assert_eq!(split("(cd /tmp && ls)"), SplitOutcome::Opaque);
    }

    #[test]
    fn unmatched_single_quote_is_opaque() {
        assert_eq!(split("echo 'unterminated"), SplitOutcome::Opaque);
    }

    #[test]
    fn unmatched_double_quote_is_opaque() {
        assert_eq!(split(r#"echo "unterminated"#), SplitOutcome::Opaque);
    }

    #[test]
    fn trailing_backslash_is_opaque() {
        assert_eq!(split("echo hi \\"), SplitOutcome::Opaque);
    }

    #[test]
    fn empty_segments_are_dropped() {
        assert_eq!(segs("find . ;; echo hi"), vec!["find .", "echo hi"]);
    }
}
