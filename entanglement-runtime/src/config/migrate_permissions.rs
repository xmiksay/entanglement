//! Self-heal a `config.yml` still carrying the retired free-form
//! `permissions:` shape (ADR-0207 stage 6c) — the config-file twin of
//! [`crate::agents::migrate`]'s agent-file migration. Runs once per
//! discovered layer file, *before* that file is read into the merge
//! ([`super::discover`]), so a legacy file is rewritten in place ahead of
//! ever reaching [`super::ceiling::RawCeiling`]'s `deny_unknown_fields`.
//!
//! Unlike the agent-file migration (a wholesale `serde_yaml::to_string`
//! re-serialize, since frontmatter carries no user prose worth preserving),
//! this does a targeted line-range splice (reusing [`super::write_key::splice`]'s
//! generic replace primitive) so a hand-commented `config.yml` keeps every
//! comment and every sibling key untouched; only the `permissions:` block's
//! own lines change.
//!
//! **Not** [`super::write_key::upsert_block`], though: its `live_block_end`
//! treats *any* column-0 comment as the end of the current block — correct
//! for the tightly controlled first-run scaffold it was built for (a
//! column-0 comment there always is the next setting's leading
//! documentation), wrong for an arbitrary hand-edited file. A real
//! `config.yml` on a real machine left the scaffold's commented example
//! lines (`#  default: allow`) sitting *between* the uncommented `permissions:`
//! key and the user's own indented rules — `upsert_block` truncated the
//! replaced range right after the key, splicing the new block in and leaving
//! the old rules dangling as extra keys of the *same* mapping (a corrupted
//! file merging both grammars) when this migration first shipped, caught by
//! the integration suite spawning the real binary against a real config.
//! [`permissions_block_end`] fixes the rule (a column-0 *comment* never ends
//! the block, only real column-0 content does) and [`migrate_if_legacy`]
//! additionally never trusts its own output blind: it re-parses the
//! rewritten text and refuses to write unless `permissions` reads back as
//! *exactly* the value it rendered.

use std::path::Path;

use anyhow::{bail, Context, Result};
use entanglement_core::Permission;
use serde_yaml::Value;

use super::{atomic::atomic_write, write_key::splice};

const NEW_GRAMMAR_KEYS: [&str; 4] = ["default", "allow", "deny", "prompt"];

/// Whether `permissions`'s parsed value is the retired free-form
/// `tool: allow|ask|deny` map rather than the new `default`/`allow`/`deny`/
/// `prompt` grammar. A non-mapping value (a scalar, a list) is left alone —
/// that's already a malformed config the real loader will reject with a
/// clear error, not this migration's problem to paper over.
fn is_legacy_shape(value: &Value) -> bool {
    let Some(map) = value.as_mapping() else {
        return false;
    };
    for (k, v) in map {
        let Some(k) = k.as_str() else {
            // A non-string key (a list/mapping key) can't happen in either
            // grammar's valid form — treat it as legacy so the loader's own
            // `deny_unknown_fields` error, not a silent pass-through, is
            // what a truly malformed file gets.
            return true;
        };
        if !NEW_GRAMMAR_KEYS.contains(&k) {
            return true;
        }
        match k {
            // The new grammar's three list keys are always sequences; a
            // legacy file spelling e.g. `allow: allow` (naming a tool
            // literally called "allow") would read one as a scalar instead.
            "allow" | "deny" | "prompt" if !v.is_sequence() => return true,
            // The new grammar spells `Permission::Ask` as `prompt`, never
            // `ask` — a legacy `default: ask` is the one shape that
            // otherwise passes every other check here.
            "default" if v.as_str() == Some("ask") => return true,
            _ => {}
        }
    }
    false
}

/// Bucket a legacy mapping's `(key, grade)` entries into the new grammar's
/// three lists, translating the one capability-class spelling that changed
/// (`call` → `exec`, ADR-0207 §3) when it names the bare class rather than a
/// scoped pattern — a scoped `call(pattern)`/`write(pattern)` key is left
/// verbatim, since the new engine already treats every scoped key as naming
/// a literal tool (§4), exactly what the old scoped spelling named too.
#[derive(Default)]
struct Converted {
    default: Option<Permission>,
    deny: Vec<String>,
    allow: Vec<String>,
    prompt: Vec<String>,
    /// Every key this couldn't translate losslessly (a scoped
    /// `call(...)`/`write(...)` rule, whose fan-out meaning changed under
    /// the new engine) — named in the migration warning for manual review.
    review: Vec<String>,
}

fn convert_legacy_mapping(map: &serde_yaml::Mapping) -> Result<Converted> {
    let mut out = Converted::default();
    for (k, v) in map {
        let key = k
            .as_str()
            .context("permissions: keys must be strings")?
            .to_string();
        let grade: Permission = serde_yaml::from_value(v.clone())
            .with_context(|| format!("permissions: invalid grade for `{key}`"))?;
        if key == "default" {
            out.default = Some(grade);
            continue;
        }
        if key.contains('(') || key.contains('{') {
            out.review.push(key.clone());
        }
        let key = if key == "call" {
            "exec".to_string()
        } else {
            key
        };
        match grade {
            Permission::Deny => out.deny.push(key),
            Permission::Allow => out.allow.push(key),
            Permission::Ask => out.prompt.push(key),
        }
    }
    Ok(out)
}

fn grade_label(p: Permission) -> &'static str {
    match p {
        Permission::Allow => "allow",
        Permission::Deny => "deny",
        Permission::Ask => "prompt",
    }
}

/// Render the new-grammar `permissions:` block as YAML text (a full `key:
/// value` block, matching [`upsert_block`]'s expectation) — via a
/// [`serde_yaml::Value`] round-trip rather than hand-formatted strings, so a
/// key needing quoting (`"bash(git *)"`) comes out correctly escaped.
fn render_block(
    default: Option<Permission>,
    deny: &[String],
    allow: &[String],
    prompt: &[String],
) -> Result<String> {
    let mut inner = serde_yaml::Mapping::new();
    if let Some(d) = default {
        inner.insert(
            Value::String("default".to_string()),
            Value::String(grade_label(d).to_string()),
        );
    }
    let as_seq =
        |list: &[String]| Value::Sequence(list.iter().map(|s| Value::String(s.clone())).collect());
    if !deny.is_empty() {
        inner.insert(Value::String("deny".to_string()), as_seq(deny));
    }
    if !allow.is_empty() {
        inner.insert(Value::String("allow".to_string()), as_seq(allow));
    }
    if !prompt.is_empty() {
        inner.insert(Value::String("prompt".to_string()), as_seq(prompt));
    }
    let mut outer = serde_yaml::Mapping::new();
    outer.insert(
        Value::String("permissions".to_string()),
        Value::Mapping(inner),
    );
    serde_yaml::to_string(&Value::Mapping(outer))
        .context("rendering the migrated permissions: block")
}

/// A segment's content without its line terminator.
fn content(segment: &str) -> &str {
    segment.trim_end_matches('\n').trim_end_matches('\r')
}

/// Whether `segment` is the live, uncommented `permissions:` top-level key
/// line (column 0, uncommented, text before the first `:` is exactly
/// `permissions`).
fn is_permissions_key(segment: &str) -> bool {
    let c = content(segment);
    if c.starts_with(char::is_whitespace) || c.starts_with('#') {
        return false;
    }
    c.split_once(':')
        .is_some_and(|(k, _)| k.trim() == "permissions")
}

/// Where the `permissions:` block ends (exclusive), starting the scan at
/// `start + 1` — mirrors [`super::write_key`]'s `live_block_end` (a trailing
/// blank line is left out, reading as the separator before whatever comes
/// next, not as part of this value) with one deliberate difference: a
/// comment at *any* indentation, column 0 included, continues the block
/// instead of ending it. A hand-edited file may leave a stray column-0
/// commented example (`#  default: allow`) between the key and the user's
/// real, indented rules — treating that as the block's end is exactly the
/// bug the module doc describes. Only a column-0 line that is neither blank
/// nor a comment — a genuine next top-level key — ends it.
fn permissions_block_end(segments: &[&str], start: usize) -> usize {
    let mut end = start + 1;
    let mut i = start + 1;
    while i < segments.len() {
        let c = content(segments[i]);
        if c.trim().is_empty() {
            i += 1;
        } else if c.starts_with(char::is_whitespace) || c.starts_with('#') {
            i += 1;
            end = i;
        } else {
            break;
        }
    }
    end
}

/// Splice `block` in as the new `permissions:` key, replacing exactly the
/// live key line through [`permissions_block_end`]'s range. `None` when no
/// live `permissions:` line is found — can't happen on the path
/// [`migrate_if_legacy`] calls this from (it already parsed a top-level
/// `permissions` value out of this same text), but returning rather than
/// panicking keeps this function honest about that precondition.
fn splice_permissions(text: &str, block: &str) -> Option<String> {
    let segments: Vec<&str> = text.split_inclusive('\n').collect();
    let start = segments.iter().position(|s| is_permissions_key(s))?;
    let end = permissions_block_end(&segments, start);
    Some(splice(&segments, start, end, block))
}

/// Self-heal `path` if its `permissions:` block still carries the retired
/// free-form shape: back it up to `<path>.bak`, rewrite the block in place
/// (every other line, including every comment, survives byte for byte), and
/// warn naming both files. A no-op when the file doesn't exist, doesn't
/// parse, or already carries the new grammar — in every one of those cases
/// the real loader either skips the file or reports its own, more specific
/// error. Never writes anything it hasn't first verified reads back
/// correctly (see the module doc) — a mismatch is a loud refusal, not a
/// best-effort write.
pub(super) fn migrate_if_legacy(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {} for permissions migration check", path.display()))?;
    let Ok(doc) = serde_yaml::from_str::<Value>(&text) else {
        // Malformed YAML: the real loader will report this with full
        // context momentarily. Not this migration's error to shadow.
        return Ok(());
    };
    let Some(perms) = doc.get("permissions") else {
        return Ok(());
    };
    if !is_legacy_shape(perms) {
        return Ok(());
    }
    let Some(map) = perms.as_mapping() else {
        return Ok(());
    };
    let converted = convert_legacy_mapping(map)?;

    let block = render_block(
        converted.default,
        &converted.deny,
        &converted.allow,
        &converted.prompt,
    )?;
    let updated = splice_permissions(&text, &block).with_context(|| {
        format!(
            "locating the live `permissions:` key in {} (already parsed as present)",
            path.display()
        )
    })?;

    // Never trust the splice blind: re-parse the rewritten text and confirm
    // `permissions` reads back as *exactly* the mapping we rendered, with
    // none of the original legacy keys surviving alongside it. This is what
    // would have caught the real corruption the module doc describes,
    // before a single byte reached disk.
    let expected = serde_yaml::from_str::<Value>(&block)
        .ok()
        .and_then(|v| v.get("permissions").cloned())
        .context("rendering the migrated permissions: block")?;
    let reparsed: Value = serde_yaml::from_str(&updated)
        .with_context(|| format!("re-parsing the rewritten {}", path.display()))?;
    if reparsed.get("permissions") != Some(&expected) {
        bail!(
            "refusing to migrate {}: the rewritten `permissions:` block didn't read back as \
             expected. This file's layout (likely a stray comment or unusual indentation inside \
             the `permissions:` block) can't be safely auto-migrated — no changes were written; \
             rewrite `permissions:` to the default/allow/deny/prompt grammar by hand instead",
            path.display()
        );
    }

    let bak = path.with_extension("yml.bak");
    std::fs::copy(path, &bak)
        .with_context(|| format!("backing up {} to {}", path.display(), bak.display()))?;
    atomic_write(path, &updated).with_context(|| {
        format!(
            "rewriting {} to the new permissions grammar",
            path.display()
        )
    })?;

    tracing::warn!(
        file = %path.display(),
        backup = %bak.display(),
        review_scoped_rules = %converted.review.join(", "),
        "config.yml `permissions:` used the retired free-form tool->grade shape (ADR-0207); \
         migrated it to the default/allow/deny/prompt mode grammar and rewrote the file — the \
         original is kept at the backup path; a scoped capability rule (`write(...)`/`call(...)`) \
         now names only the literal tool, never a capability fan-out, so review those by hand",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_grammar_is_not_legacy() {
        let v: Value = serde_yaml::from_str("default: prompt\ndeny: [write]\n").unwrap();
        assert!(!is_legacy_shape(&v));
    }

    #[test]
    fn free_form_tool_map_is_legacy() {
        let v: Value = serde_yaml::from_str("bash: ask\ndefault: allow\n").unwrap();
        assert!(is_legacy_shape(&v));
    }

    #[test]
    fn legacy_default_ask_spelling_is_legacy() {
        let v: Value = serde_yaml::from_str("default: ask\n").unwrap();
        assert!(is_legacy_shape(&v));
    }

    #[test]
    fn a_scalar_allow_key_value_is_legacy() {
        // `allow: allow` — a legacy rule for a literal tool named "allow" —
        // is not a sequence, so it can't be the new grammar's `allow:` list.
        let v: Value = serde_yaml::from_str("allow: allow\n").unwrap();
        assert!(is_legacy_shape(&v));
    }

    #[test]
    fn migrate_rewrites_in_place_and_keeps_a_backup_with_comments_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");
        std::fs::write(
            &path,
            "# my config\nagent: build\n\n# ceiling\npermissions:\n  default: allow\n  bash: ask\n  edit: deny\n\nverbose: true\n",
        )
        .unwrap();

        migrate_if_legacy(&path).unwrap();

        let bak = path.with_extension("yml.bak");
        assert!(bak.exists(), "backup must be kept");
        let original = std::fs::read_to_string(&bak).unwrap();
        assert!(original.contains("bash: ask"));

        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(rewritten.contains("# my config"), "{rewritten}");
        assert!(rewritten.contains("agent: build"), "{rewritten}");
        assert!(rewritten.contains("# ceiling"), "{rewritten}");
        assert!(rewritten.contains("verbose: true"), "{rewritten}");
        assert!(!rewritten.contains("bash: ask"), "{rewritten}");

        let doc: Value = serde_yaml::from_str(&rewritten).unwrap();
        let perms = doc.get("permissions").unwrap();
        assert!(!is_legacy_shape(perms), "{perms:?}");
        let deny = perms.get("deny").unwrap().as_sequence().unwrap();
        assert!(deny.contains(&Value::String("edit".to_string())));
        let prompt = perms.get("prompt").unwrap().as_sequence().unwrap();
        assert!(prompt.contains(&Value::String("bash".to_string())));
    }

    /// Regression for the real corruption this module's doc describes: a
    /// live `permissions:` key followed by *stray, commented* scaffold
    /// example lines (`#  default: allow`) — leftovers a user never
    /// deleted — before their real, indented rules. `write_key::upsert_block`
    /// treated the first commented line as the block's end and spliced the
    /// new block in right after the bare key, leaving the old rules
    /// (including the very keys the new `deny`/`allow`/`prompt` lists now
    /// also carry) dangling as extra entries of the *same* mapping — a file
    /// valid under neither grammar. The fix must replace the *entire* block,
    /// comments included, and every one of the original per-tool keys must
    /// be gone afterward.
    #[test]
    fn migrate_handles_a_stray_commented_example_inside_the_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");
        std::fs::write(
            &path,
            "agent: plan\n\npermissions:\n#  default: allow\n#  bash: ask\n  bash(git **): allow\n  edit: deny\n\nmcp:\n  chess:\n    url: https://example.com/mcp\n",
        )
        .unwrap();

        migrate_if_legacy(&path).unwrap();

        let rewritten = std::fs::read_to_string(&path).unwrap();
        let doc: Value = serde_yaml::from_str(&rewritten).unwrap_or_else(|e| {
            panic!("rewritten file must still be valid YAML: {e}\n{rewritten}")
        });
        // Sibling keys before and after the block survive untouched.
        assert_eq!(doc.get("agent").and_then(Value::as_str), Some("plan"));
        assert!(doc.get("mcp").is_some(), "{rewritten}");

        let perms = doc.get("permissions").expect("permissions key survives");
        assert!(
            !is_legacy_shape(perms),
            "must read back as the new grammar: {perms:?}\nfull file:\n{rewritten}"
        );
        // The critical assertion: the old per-tool keys must not survive
        // alongside the new lists — that coexistence *is* the corruption.
        assert!(perms.get("bash(git **)").is_none(), "{rewritten}");
        assert!(perms.get("edit").is_none(), "{rewritten}");
        let allow = perms.get("allow").and_then(Value::as_sequence);
        assert!(
            allow.is_some_and(|a| a.contains(&Value::String("bash(git **)".to_string()))),
            "{rewritten}"
        );
        let deny = perms.get("deny").and_then(Value::as_sequence);
        assert!(
            deny.is_some_and(|d| d.contains(&Value::String("edit".to_string()))),
            "{rewritten}"
        );
    }

    #[test]
    fn migrate_translates_bare_call_to_exec() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");
        std::fs::write(&path, "permissions:\n  call: deny\n").unwrap();
        migrate_if_legacy(&path).unwrap();
        let rewritten = std::fs::read_to_string(&path).unwrap();
        let doc: Value = serde_yaml::from_str(&rewritten).unwrap();
        let deny = doc
            .get("permissions")
            .unwrap()
            .get("deny")
            .unwrap()
            .as_sequence()
            .unwrap();
        assert!(deny.contains(&Value::String("exec".to_string())));
        assert!(!deny.contains(&Value::String("call".to_string())));
    }

    #[test]
    fn already_new_grammar_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");
        let text = "permissions:\n  default: prompt\n  deny: [write]\n";
        std::fs::write(&path, text).unwrap();
        migrate_if_legacy(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(!path.with_extension("yml.bak").exists());
    }

    #[test]
    fn no_permissions_key_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");
        std::fs::write(&path, "agent: build\n").unwrap();
        migrate_if_legacy(&path).unwrap();
        assert!(!path.with_extension("yml.bak").exists());
    }

    #[test]
    fn missing_file_is_a_noop() {
        let path = Path::new("/nonexistent/config.yml");
        migrate_if_legacy(path).unwrap();
    }
}
