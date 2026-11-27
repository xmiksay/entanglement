//! Brace-set expansion for the `glob`/`grep` search walk (ADR-0150):
//! `a{b,c}d` → `abd` + `acd`, cartesian across groups, recursive into nested
//! groups. The `glob` crate has no brace token, so `**/*.{rs,md}` silently
//! matched nothing before this pass — one of the top causes of the empty-result
//! rate that motivated ADR-0150.

use anyhow::{bail, Result};

/// Cap on the number of expanded alternatives — past this the input is a
/// pathological cartesian explosion, not a real search.
const MAX_BRACE_ALTERNATIVES: usize = 64;

/// Expand every brace group in `pattern`. No group → the pattern verbatim.
///
/// Bash semantics for the corner cases: a group needs a top-level comma to
/// expand (`{abc}` stays literal), an unmatched `{`/`}` stays literal
/// (matching the `glob` crate's own treatment, so odd inputs regress to the
/// old behavior instead of erroring), and an empty alternative is allowed
/// (`a{,b}` → `a` + `ab`). There is no escape syntax — a literal `{` in a
/// filename can still be matched via a `[{]` character class.
pub(crate) fn expand_braces(pattern: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    expand_into(pattern, &mut out)?;
    Ok(out)
}

fn expand_into(pattern: &str, out: &mut Vec<String>) -> Result<()> {
    let Some((start, end)) = find_group(pattern) else {
        if out.len() >= MAX_BRACE_ALTERNATIVES {
            bail!("brace expansion exceeds {MAX_BRACE_ALTERNATIVES} alternatives");
        }
        out.push(pattern.to_string());
        return Ok(());
    };
    let prefix = &pattern[..start];
    let suffix = &pattern[end + 1..];
    for alt in split_alternatives(&pattern[start + 1..end]) {
        expand_into(&format!("{prefix}{alt}{suffix}"), out)?;
    }
    Ok(())
}

/// First balanced `{...}` containing a depth-1 comma, as byte offsets of the
/// braces. Comma-less and unmatched groups are skipped (they stay literal).
fn find_group(pattern: &str) -> Option<(usize, usize)> {
    let bytes = pattern.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] != b'{' {
            continue;
        }
        let mut depth = 0usize;
        let mut has_comma = false;
        for (j, &b) in bytes.iter().enumerate().skip(i) {
            match b {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        if has_comma {
                            return Some((i, j));
                        }
                        break;
                    }
                }
                b',' if depth == 1 => has_comma = true,
                _ => {}
            }
        }
    }
    None
}

/// Split a group body on its depth-0 commas (commas inside nested groups
/// belong to the nested group).
fn split_alternatives(body: &str) -> Vec<&str> {
    let mut alts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (idx, b) in body.bytes().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                alts.push(&body[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    alts.push(&body[start..]);
    alts
}

#[cfg(test)]
mod tests {
    use super::expand_braces;

    fn expect(pattern: &str, want: &[&str]) {
        assert_eq!(expand_braces(pattern).unwrap(), want);
    }

    #[test]
    fn no_brace_passthrough() {
        expect("**/*.rs", &["**/*.rs"]);
        expect("", &[""]);
    }

    #[test]
    fn two_alternatives() {
        expect("**/*.{rs,md}", &["**/*.rs", "**/*.md"]);
    }

    #[test]
    fn nested_braces() {
        expect("{a,{b,c}}", &["a", "b", "c"]);
    }

    #[test]
    fn cartesian_across_two_groups() {
        expect("{a,b}/{c,d}", &["a/c", "a/d", "b/c", "b/d"]);
    }

    #[test]
    fn empty_alternative() {
        expect("a{,b}", &["a", "ab"]);
    }

    #[test]
    fn unmatched_brace_is_literal() {
        expect("a{b,c", &["a{b,c"]);
        expect("a}b", &["a}b"]);
    }

    #[test]
    fn comma_less_group_is_literal() {
        expect("a{bc}d", &["a{bc}d"]);
    }

    #[test]
    fn comma_less_outer_still_expands_inner() {
        // Bash agrees: `{ab{c,d}}` → `{abc}` `{abd}`.
        expect("{ab{c,d}}", &["{abc}", "{abd}"]);
    }

    #[test]
    fn explosion_capped() {
        // 2^7 = 128 leaves > 64.
        let p = "{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}";
        let err = expand_braces(p).unwrap_err().to_string();
        assert!(err.contains("64"), "unexpected error: {err}");
    }
}
