//! Prompt-template expansion (#733): `$ARGUMENTS` and `$1`..`$9`. Pure text —
//! no shell execution, no file inclusion. Claude Code's `` !`cmd` `` and
//! `@file` forms are deliberately unsupported and pass through literally, so
//! a command file can never run or read anything by being invoked.

/// Split an argument string shell-style: whitespace separates, `'…'` and
/// `"…"` group (quotes removed), a backslash escapes the next character
/// outside single quotes (inside double quotes only `\"` and `\\`). An
/// unterminated quote runs to the end of the input rather than failing — a
/// typo in a prompt must not swallow the whole invocation.
pub fn split_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    let mut chars = args.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' && matches!(chars.peek(), Some('"' | '\\')) => {
                cur.extend(chars.next());
            }
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                in_token = true;
            }
            None if c == '\\' => {
                cur.extend(chars.next());
                in_token = true;
            }
            None if c.is_whitespace() => {
                if in_token {
                    out.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            None => {
                cur.push(c);
                in_token = true;
            }
        }
    }
    if in_token {
        out.push(cur);
    }
    out
}

/// Render a command body for `args`: `$ARGUMENTS` becomes the whole
/// (trimmed) argument string, `$1`..`$9` the positional args from
/// [`split_args`] (empty when missing); any other `$` is literal. A body with
/// no placeholder at all still gets non-empty args, appended as an
/// `ARGUMENTS:` paragraph — dropping what the user typed would be silent
/// data loss.
pub fn render(body: &str, args: &str) -> String {
    let args = args.trim();
    render_with(body, args, &split_args(args))
}

/// [`render`] with the arguments already tokenized: `args` fills
/// `$ARGUMENTS` (and the no-placeholder paragraph), `positional` fills
/// `$1`..`$9`. The `run` head passes its argv straight through (#798), so the
/// shell's own quoting decides the tokens and nothing is re-quoted.
pub fn render_with(body: &str, args: &str, positional: &[String]) -> String {
    let mut out = String::with_capacity(body.len() + args.len());
    let mut placeholder = false;
    let mut rest = body;
    // One left-to-right pass over the *body* only: substituted text is never
    // rescanned, so an argument that itself contains `$1` stays literal.
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(tail) = after.strip_prefix("ARGUMENTS") {
            out.push_str(args);
            placeholder = true;
            rest = tail;
        } else if let Some(d) = after.chars().next().filter(|c| ('1'..='9').contains(c)) {
            let idx = d as usize - '1' as usize;
            out.push_str(positional.get(idx).map_or("", String::as_str));
            placeholder = true;
            rest = &after[1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    let mut out = out.trim().to_string();
    if !placeholder && !args.is_empty() {
        out.push_str("\n\nARGUMENTS: ");
        out.push_str(args);
    }
    out
}

/// Split `/name rest…` into `(name, rest)`. `None` when the input is not a
/// slash invocation (no leading `/`, or a bare `/`).
pub fn parse_invocation(input: &str) -> Option<(&str, &str)> {
    let rest = input.trim_start().strip_prefix('/')?;
    let (name, args) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    (!name.is_empty()).then_some((name, args.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_respects_quotes_and_escapes() {
        assert_eq!(split_args("a b  c"), vec!["a", "b", "c"]);
        assert_eq!(split_args(r#""a b" 'c d' e"#), vec!["a b", "c d", "e"]);
        assert_eq!(split_args(r"a\ b c"), vec!["a b", "c"]);
        assert_eq!(split_args(r#""say \"hi\"""#), vec![r#"say "hi""#]);
        assert_eq!(split_args(r"'no \escape'"), vec![r"no \escape"]);
        assert_eq!(split_args(r#""" x"#), vec!["", "x"]);
        assert!(split_args("   ").is_empty());
    }

    #[test]
    fn unterminated_quote_runs_to_the_end() {
        assert_eq!(split_args(r#"a "b c"#), vec!["a", "b c"]);
    }

    #[test]
    fn arguments_placeholder_takes_the_whole_string() {
        assert_eq!(
            render("Review $ARGUMENTS now", "  src/a.rs  \"x y\" "),
            r#"Review src/a.rs  "x y" now"#
        );
    }

    #[test]
    fn positional_placeholders_and_missing_args() {
        let body = "from $1 to $2, then $3.";
        assert_eq!(
            render(body, r#"main "feat x""#),
            "from main to feat x, then ."
        );
        // `$0`, `$x` and a trailing `$` stay literal; `$10` is `$1` then `0`.
        assert_eq!(render("$0 $x $ $10", "a"), "$0 $x $ a0");
    }

    #[test]
    fn substituted_text_is_not_rescanned() {
        assert_eq!(render("<$ARGUMENTS>", "$1 $ARGUMENTS"), "<$1 $ARGUMENTS>");
    }

    #[test]
    fn no_placeholder_appends_the_arguments() {
        assert_eq!(render("Do it.\n", ""), "Do it.");
        assert_eq!(render("Do it.", "fast"), "Do it.\n\nARGUMENTS: fast");
    }

    #[test]
    fn shell_and_file_forms_pass_through_literally() {
        let body = "Status: !`git status` and @src/main.rs";
        assert_eq!(render(body, ""), body);
    }

    #[test]
    fn render_with_takes_pre_split_arguments_verbatim() {
        let argv = ["a b".to_string(), "it's".to_string()];
        assert_eq!(render_with("[$1] [$2]", "a b it's", &argv), "[a b] [it's]");
        assert_eq!(render_with("$ARGUMENTS", "it's", &argv[1..]), "it's");
        assert_eq!(render_with("Hi", "a b", &argv[..1]), "Hi\n\nARGUMENTS: a b");
    }

    #[test]
    fn invocation_parsing() {
        assert_eq!(parse_invocation("/review a b"), Some(("review", "a b")));
        assert_eq!(parse_invocation("/fe:review"), Some(("fe:review", "")));
        assert_eq!(parse_invocation("/x\nline two"), Some(("x", "line two")));
        assert_eq!(parse_invocation("/"), None);
        assert_eq!(parse_invocation("review"), None);
    }
}
