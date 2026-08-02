//! User-override file resolution and the pre-deserialization deep merge.
//!
//! Split out of `catalog.rs` for the 400-line file cap, along the seam the
//! module doc already draws: `catalog.rs` owns the catalog *types* and their
//! resolution, this file owns *where the user's YAML comes from* and *how it is
//! folded onto the embedded defaults*. Merge semantics — keyed sequences,
//! key-wise mappings, scalar replacement — are documented on `catalog.rs`'s
//! module doc, since they are what a reader of `Catalog` needs to know.

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
    if let Some(p) = std::env::var_os(PROVIDERS_FILE_ENV) {
        return Some(CatalogFile {
            path: PathBuf::from(p),
            explicit: true,
        });
    }
    dirs::config_dir().map(|d| CatalogFile {
        path: d.join("entanglement").join("providers.yml"),
        explicit: false,
    })
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
