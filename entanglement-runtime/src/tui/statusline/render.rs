//! Template rendering for the status line (#762): placeholder substitution,
//! the collapse rule for empty values, and the narrow-terminal shed order.
//! Pure string work over [`Piece`]s so every rule is testable without a
//! terminal; the caller maps each [`Slot`] to a theme style.

use std::collections::HashMap;

use unicode_width::UnicodeWidthStr;

use crate::config::statusline::Token;

/// The theme slot a rendered piece is styled with (`StatusTheme` fields).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    Repo,
    Path,
    Branch,
    Worktree,
    Text,
    Dim,
    Attention,
}

pub(crate) type Piece = (String, Slot);
/// Placeholder values; a missing key renders like an empty value.
pub(crate) type Vars = HashMap<&'static str, String>;

/// Placeholders shed (emptied) one group at a time, after the session
/// counters, while the line overflows. `worktree` and `branch` follow, then
/// the path is middle-elided; `repo` is never shed — it is the one fact the
/// line exists to show.
const SHED: &[&[&str]] = &[
    &["tasks", "cost", "context_pct"],
    &["action", "model", "mode", "agent"],
    &["session"],
];

fn slot_for(name: &str) -> Slot {
    match name {
        "repo" => Slot::Repo,
        "cwd" | "cwd_rel" => Slot::Path,
        "branch" => Slot::Branch,
        "worktree" => Slot::Worktree,
        "session" => Slot::Dim,
        _ => Slot::Text,
    }
}

/// Substitute `vars` into `tokens`, collapsing empty values: each literal
/// run is the *prefix* of the placeholder after it, shown only when that
/// placeholder has a value and either something before it was shown or it
/// is the template's first placeholder; the run after the last placeholder
/// is that placeholder's suffix. So `{repo} › {cwd_rel}  ⎇ {branch}` at a
/// repo root reads `repo  ⎇ main`, and with nothing but `{repo}` set, `repo`.
/// A template with no placeholders renders verbatim.
pub(crate) fn render(tokens: &[Token], vars: &Vars) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut prefix: Option<&str> = None;
    let mut seen_var = false;
    let mut shown_any = false;
    let mut last_shown = false;
    for token in tokens {
        match token {
            Token::Lit(text) => prefix = Some(text),
            Token::Var(name) => {
                let value = vars.get(name).map(String::as_str).unwrap_or("");
                last_shown = !value.is_empty();
                if last_shown {
                    if let Some(p) = prefix.filter(|_| shown_any || !seen_var) {
                        out.push((p.to_string(), Slot::Dim));
                    }
                    out.push((value.to_string(), slot_for(name)));
                    shown_any = true;
                }
                prefix = None;
                seen_var = true;
            }
        }
    }
    if let Some(suffix) = prefix.filter(|_| !seen_var || last_shown) {
        out.push((suffix.to_string(), Slot::Dim));
    }
    out
}

pub(crate) fn width(pieces: &[Piece]) -> usize {
    pieces.iter().map(|(s, _)| s.width()).sum()
}

/// Columns the row needs: left, a one-column gap, right.
fn row_width(left: &[Piece], right: &[Piece]) -> usize {
    match (width(left), width(right)) {
        (l, 0) => l,
        (0, r) => r,
        (l, r) => l + 1 + r,
    }
}

/// Fit the row into `cols`: drop `counters`, then shed [`SHED`], shorten
/// `[worktree: name]` to `[worktree]`, drop worktree, drop branch, then
/// middle-elide `{cwd_rel}` and `{cwd}`. Returns `(left, right)`; whatever
/// still overflows after the last step is clipped by the terminal.
pub(crate) fn layout(
    left: &[Token],
    right: &[Token],
    vars: &Vars,
    counters: Vec<Piece>,
    cols: usize,
) -> (Vec<Piece>, Vec<Piece>) {
    let mut vars = vars.clone();
    let mut counters = counters;
    let mut stage = 0usize;
    loop {
        let l = render(left, &vars);
        let mut r = render(right, &vars);
        r.extend(counters.iter().cloned());
        let overflow = row_width(&l, &r).saturating_sub(cols);
        if overflow == 0 {
            return (l, r);
        }
        match stage {
            0 => counters.clear(),
            1..=3 => SHED[stage - 1].iter().for_each(|n| {
                vars.remove(n);
            }),
            4 => {
                if vars.get("worktree").is_some_and(|w| !w.is_empty()) {
                    vars.insert("worktree", "[worktree]".to_string());
                }
            }
            5 => {
                vars.remove("worktree");
            }
            6 => {
                vars.remove("branch");
            }
            7 | 8 => {
                let key = if stage == 7 { "cwd_rel" } else { "cwd" };
                if let Some(path) = vars.get(key) {
                    let target = path.width().saturating_sub(overflow);
                    let elided = middle_elide(path, target);
                    vars.insert(key, elided);
                }
            }
            _ => return (l, r),
        }
        stage += 1;
    }
}

/// Shorten `path` to at most `max` columns keeping its tail: `…/deep/dir`,
/// then `…/dir`, then a bare `…` (which may still exceed a `max` of 0 —
/// the terminal clips it).
pub(crate) fn middle_elide(path: &str, max: usize) -> String {
    if path.width() <= max {
        return path.to_string();
    }
    let parts: Vec<&str> = path.split('/').collect();
    (1..parts.len())
        .rev()
        .map(|keep| format!("…/{}", parts[parts.len() - keep..].join("/")))
        .find(|candidate| candidate.width() <= max)
        .unwrap_or_else(|| "…".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::statusline::tokenize;

    const DEFAULT: &str = "{repo} › {cwd_rel}  ⎇ {branch} {worktree}";

    fn vars(pairs: &[(&'static str, &str)]) -> Vars {
        pairs.iter().map(|(k, v)| (*k, v.to_string())).collect()
    }

    fn text(pieces: &[Piece]) -> String {
        pieces.iter().map(|(s, _)| s.as_str()).collect()
    }

    fn line(tpl: &str, v: &Vars) -> String {
        text(&render(&tokenize(tpl).0, v))
    }

    fn full() -> Vars {
        vars(&[
            ("repo", "entanglement"),
            ("cwd_rel", "crates/runtime/src/tui"),
            ("branch", "feat/762"),
            ("worktree", "[worktree: wt1]"),
            ("session", "a1b2c3d4"),
        ])
    }

    #[test]
    fn substitutes_every_value() {
        assert_eq!(
            line(DEFAULT, &full()),
            "entanglement › crates/runtime/src/tui  ⎇ feat/762 [worktree: wt1]"
        );
    }

    #[test]
    fn empty_values_collapse_with_their_separator() {
        let v = vars(&[("repo", "proj"), ("branch", "main")]);
        assert_eq!(line(DEFAULT, &v), "proj  ⎇ main", "repo root, no worktree");
        assert_eq!(line(DEFAULT, &vars(&[("repo", "~/notes")])), "~/notes");
        // A leading empty placeholder takes the separator after it along.
        assert_eq!(line("{action} · {repo}", &vars(&[("repo", "r")])), "r");
        // Prefix of the first placeholder and the trailing suffix follow it.
        let t = "[{tasks}] {repo}!";
        assert_eq!(line(t, &vars(&[("repo", "r")])), "r!");
        assert_eq!(
            line(t, &vars(&[("repo", "r"), ("tasks", "1/3")])),
            "[1/3] r!"
        );
        assert_eq!(line("{repo} ({tasks})", &vars(&[("repo", "r")])), "r");
    }

    #[test]
    fn literal_only_and_unknown_placeholders_render_verbatim() {
        assert_eq!(line("just text", &Vars::new()), "just text");
        assert_eq!(line("{repo} {nope}", &vars(&[("repo", "r")])), "r {nope}");
    }

    #[test]
    fn slots_follow_the_placeholder() {
        let pieces = render(&tokenize("{repo} › {branch}").0, &full());
        let slots: Vec<Slot> = pieces.iter().map(|p| p.1).collect();
        assert_eq!(slots, vec![Slot::Repo, Slot::Dim, Slot::Branch]);
    }

    fn fit(cols: usize, counters: bool) -> (String, String) {
        let c = if counters {
            vec![(" (3 sessions)".to_string(), Slot::Dim)]
        } else {
            Vec::new()
        };
        let (l, r) = layout(
            &tokenize(DEFAULT).0,
            &tokenize("{session}").0,
            &full(),
            c,
            cols,
        );
        assert!(row_width(&l, &r) <= cols || cols < 14, "{cols}: overflow");
        (text(&l), text(&r))
    }

    #[test]
    fn truncation_sheds_in_the_approved_order() {
        let wide = fit(200, true);
        assert_eq!(wide.1, "a1b2c3d4 (3 sessions)");
        // 65 left + gap + 8 session = 74: the counters go first.
        assert_eq!(fit(74, true).1, "a1b2c3d4");
        // Then the session.
        let (l, r) = fit(70, true);
        assert_eq!((l.as_str(), r.as_str()), (wide.0.as_str(), ""));
        // Then the worktree's name, then the worktree marker.
        assert!(
            fit(62, true).0.ends_with("[worktree]"),
            "{:?}",
            fit(62, true)
        );
        assert_eq!(
            fit(55, true).0,
            "entanglement › crates/runtime/src/tui  ⎇ feat/762"
        );
        // Then the branch, then the path is middle-elided.
        assert_eq!(fit(40, true).0, "entanglement › crates/runtime/src/tui");
        assert_eq!(fit(30, true).0, "entanglement › …/src/tui");
        assert_eq!(fit(20, true).0, "entanglement › …/tui");
        assert_eq!(fit(16, true).0, "entanglement › …");
        // The repo name is never dropped, even past the last step.
        assert!(fit(5, true).0.starts_with("entanglement"));
    }

    #[test]
    fn middle_elide_keeps_the_tail() {
        assert_eq!(middle_elide("a/b/c/deep/dir", 99), "a/b/c/deep/dir");
        assert_eq!(middle_elide("a/b/c/deep/dir", 12), "…/c/deep/dir");
        assert_eq!(middle_elide("a/b/c/deep/dir", 11), "…/deep/dir");
        assert_eq!(middle_elide("a/b/c/deep/dir", 5), "…/dir");
        assert_eq!(middle_elide("a/b/c/deep/dir", 2), "…");
        assert_eq!(middle_elide("~/x/longname", 10), "…/longname");
    }
}
