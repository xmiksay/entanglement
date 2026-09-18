//! Duplicate-JSON-key detection for a model's raw tool-call input (#560
//! remainder): `serde_json::Value` deserializes an object by inserting each
//! key in turn, so a repeated key silently keeps only the *later* value —
//! `{"path":"file.md","path":"data"}` becomes `{"path":"data"}` with no trace
//! the first `path` (meant, say, as `content`) ever existed. [`validate`]
//! only ever sees the collapsed `Value`, so it can report "missing
//! `content`" but never *why* — this module re-scans the raw text before
//! collapse and names the key that got silently overwritten.
//!
//! This parses **untrusted model output**, so it must never panic: malformed
//! JSON simply yields no duplicates (`validate`'s `malformed_json` branch
//! already reports that separately) rather than an error path here.

use std::cell::RefCell;
use std::rc::Rc;

use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};

/// Scan `input` for JSON object keys repeated within the same object, at any
/// nesting depth, and return their names (sorted, deduped — a key repeated
/// three times, or at two different nesting levels, is named once). Empty on
/// malformed JSON or anything with no duplicates.
pub fn duplicate_keys(input: &str) -> Vec<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let found = Rc::new(RefCell::new(Vec::new()));
    let mut de = serde_json::Deserializer::from_str(trimmed);
    // A parse error here is not this function's concern — `validate` already
    // reports malformed JSON as its own violation.
    let _ = de.deserialize_any(DupVisitor {
        found: found.clone(),
    });
    let mut out = Rc::try_unwrap(found)
        .map(RefCell::into_inner)
        .unwrap_or_default();
    out.sort();
    out.dedup();
    out
}

/// Walks any JSON value, recording a duplicate key the moment `MapAccess`
/// yields it twice at the same object level, then recurses into every
/// value (object or array) via [`DupSeed`] so a nested object's duplicates
/// are found too. Scalars are ignored — nothing to recurse into.
struct DupVisitor {
    found: Rc<RefCell<Vec<String>>>,
}

impl<'de> Visitor<'de> for DupVisitor {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen = std::collections::HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                self.found.borrow_mut().push(key);
            }
            map.next_value_seed(DupSeed {
                found: self.found.clone(),
            })?;
        }
        Ok(())
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq
            .next_element_seed(DupSeed {
                found: self.found.clone(),
            })?
            .is_some()
        {}
        Ok(())
    }

    // Scalars carry no keys of their own — every other `visit_*` is the
    // trait's provided no-op default.
    fn visit_bool<E>(self, _v: bool) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_i64<E>(self, _v: i64) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_u64<E>(self, _v: u64) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_f64<E>(self, _v: f64) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_str<E>(self, _v: &str) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_string<E>(self, _v: String) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }
}

/// [`DupVisitor`] as a `DeserializeSeed` so it can be handed to
/// `next_value_seed`/`next_element_seed` and recurse into nested
/// objects/arrays, sharing the same accumulator via the cloned `Rc`.
struct DupSeed {
    found: Rc<RefCell<Vec<String>>>,
}

impl<'de> DeserializeSeed<'de> for DupSeed {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(DupVisitor { found: self.found })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_duplicates_is_empty() {
        assert!(duplicate_keys(r#"{"path":"a","content":"b"}"#).is_empty());
    }

    #[test]
    fn top_level_duplicate_is_named() {
        assert_eq!(
            duplicate_keys(r#"{"path":"file.md","path":"data"}"#),
            vec!["path".to_string()]
        );
    }

    #[test]
    fn repeated_three_times_is_named_once() {
        assert_eq!(
            duplicate_keys(r#"{"a":1,"a":2,"a":3}"#),
            vec!["a".to_string()]
        );
    }

    #[test]
    fn nested_duplicate_is_found() {
        assert_eq!(
            duplicate_keys(r#"{"outer":{"path":"a","path":"b"},"fine":1}"#),
            vec!["path".to_string()]
        );
    }

    #[test]
    fn duplicate_inside_an_array_element_is_found() {
        assert_eq!(
            duplicate_keys(r#"{"items":[{"x":1,"x":2}]}"#),
            vec!["x".to_string()]
        );
    }

    #[test]
    fn multiple_distinct_duplicates_are_all_named_sorted() {
        assert_eq!(
            duplicate_keys(r#"{"b":1,"b":2,"a":1,"a":2}"#),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn malformed_json_yields_no_duplicates() {
        assert!(duplicate_keys("{not json").is_empty());
    }

    #[test]
    fn empty_input_yields_no_duplicates() {
        assert!(duplicate_keys("").is_empty());
        assert!(duplicate_keys("   ").is_empty());
    }

    #[test]
    fn non_object_top_level_yields_no_duplicates() {
        assert!(duplicate_keys("[1,2,3]").is_empty());
        assert!(duplicate_keys("\"just a string\"").is_empty());
    }
}
