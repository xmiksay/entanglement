//! User-override file resolution and the pre-deserialization deep merge.
//!
//! Split out of `catalog.rs` for the 400-line file cap, along the seam the
//! module doc already draws: `catalog.rs` owns the catalog *types* and their
//! resolution, this file owns *where the user's YAML comes from* and *how it is
//! folded onto the embedded defaults*. Merge semantics — keyed sequences,
//! key-wise mappings, scalar replacement — are documented on `catalog.rs`'s
//! module doc, since they are what a reader of `Catalog` needs to know.

use std::ffi::OsString;
use std::path::PathBuf;

use serde_yaml::Value;

use super::PROVIDERS_FILE_ENV;

/// A resolved catalog-file location plus whether it came from an explicit
/// `ENTANGLEMENT_PROVIDERS_FILE` override (vs the default `${config_dir}` path),
/// so [`Catalog::load`](super::Catalog::load) can be loud about a missing
/// *explicit* override while staying quiet about a missing *default* (#204).
pub(super) struct CatalogFile {
    pub(super) path: PathBuf,
    pub(super) explicit: bool,
}

/// The user override file path: `${config_dir}/entanglement/providers.yml`,
/// overridable via `ENTANGLEMENT_PROVIDERS_FILE`.
pub(super) fn providers_file_path() -> Option<CatalogFile> {
    resolve_providers_file(|name| std::env::var_os(name), default_config_dir)
}

/// [`providers_file_path`] over an injected env lookup and config dir, so the
/// precedence is unit-testable without mutating process env (#773).
pub(super) fn resolve_providers_file(
    env: impl Fn(&str) -> Option<OsString>,
    config_dir: impl FnOnce() -> Option<PathBuf>,
) -> Option<CatalogFile> {
    if let Some(p) = env(PROVIDERS_FILE_ENV) {
        return Some(CatalogFile {
            path: PathBuf::from(p),
            explicit: true,
        });
    }
    config_dir().map(|d| CatalogFile {
        path: d.join("entanglement").join("providers.yml"),
        explicit: false,
    })
}

fn default_config_dir() -> Option<PathBuf> {
    // Guard (#773): a unit test reaching the real config dir would merge the
    // developer's own `providers.yml` into the catalog under test — fail it
    // loudly so the test uses `Catalog::builtin`/`Catalog::load_from` instead.
    if cfg!(test) {
        panic!("test resolved the real providers.yml; use Catalog::builtin or Catalog::load_from");
    }
    dirs::config_dir()
}

/// Drop provider/model keys a released-or-branch catalog once accepted but no
/// longer carries, so `deny_unknown_fields` doesn't turn an old user file into
/// a startup error. `advertise_discovered` (ADR-0200) was retired by
/// ADR-0204; `discovery` and `tool_advertising` were then both retired whole
/// by ADR-0226 (one advertised surface, no knob), as was a bundled MCP
/// server's three-state `state:` (registered = available) — whose `disabled`
/// value becomes `disabled: true`, so an old opt-out never silently flips a
/// server back on.
pub(super) fn strip_retired_keys(mut doc: Value) -> Value {
    const RETIRED: &[(&str, &str)] = &[
        ("advertise_discovered", "ADR-0204"),
        ("discovery", "ADR-0226"),
        ("tool_advertising", "ADR-0226"),
    ];
    let strip = |entry: &mut Value, scope: &str| {
        let Some(map) = entry.as_mapping_mut() else {
            return;
        };
        for (key, adr) in RETIRED {
            if map.remove(*key).is_some() {
                tracing::warn!("ignoring retired {scope} catalog key `{key}` ({adr})");
            }
        }
    };
    let retire_mcp_state = |server: &mut Value| {
        let Some(map) = server.as_mapping_mut() else {
            return;
        };
        if let Some(state) = map.remove("state") {
            tracing::warn!("ignoring retired MCP server catalog key `state` (ADR-0226)");
            if state.as_str() == Some("disabled") {
                map.insert(Value::from("disabled"), Value::Bool(true));
            }
        }
    };
    if let Some(providers) = doc.get_mut("providers").and_then(Value::as_sequence_mut) {
        for provider in providers {
            strip(provider, "provider");
            if let Some(models) = provider.get_mut("models").and_then(Value::as_sequence_mut) {
                for model in models {
                    strip(model, "model");
                }
            }
            if let Some(servers) = provider
                .get_mut("mcp_servers")
                .and_then(Value::as_mapping_mut)
            {
                for (_, server) in servers.iter_mut() {
                    retire_mcp_state(server);
                }
            }
        }
    }
    doc
}

/// Deep-merge `over` onto `base`. Mappings merge key-wise; the two keyed
/// sequences (`providers` by `name`, `models` by `id`) merge by identity;
/// everything else is replaced by `over`.
pub(super) fn merge_value(base: Value, over: Value) -> Value {
    match (base, over) {
        (Value::Mapping(mut base_map), Value::Mapping(over_map)) => {
            for (key, over_val) in over_map {
                let merged = match base_map.remove(&key) {
                    Some(base_val) => match key.as_str() {
                        Some("providers") => merge_seq_by(base_val, over_val, "name"),
                        Some("models") => merge_seq_by(base_val, over_val, "id"),
                        _ => merge_value(base_val, over_val),
                    },
                    None => over_val,
                };
                base_map.insert(key, merged);
            }
            Value::Mapping(base_map)
        }
        // Scalars and non-keyed sequences: the user value wins outright.
        (_, over) => over,
    }
}

/// Merge two sequences by a shared identity key: matching entries merge
/// recursively (in the base's position), user-only entries append. On a type
/// mismatch (either side isn't a sequence) the user value wins outright.
fn merge_seq_by(base: Value, over: Value, id_key: &str) -> Value {
    let mut base_seq = match base {
        Value::Sequence(s) => s,
        _ => return over,
    };
    let over_seq = match over {
        Value::Sequence(s) => s,
        other => return other,
    };
    for over_item in over_seq {
        let over_id = over_item.get(id_key).cloned();
        let pos = over_id
            .as_ref()
            .and_then(|oid| base_seq.iter().position(|b| b.get(id_key) == Some(oid)));
        match pos {
            Some(i) => {
                let base_item = base_seq.remove(i);
                base_seq.insert(i, merge_value(base_item, over_item));
            }
            None => base_seq.push(over_item),
        }
    }
    Value::Sequence(base_seq)
}
