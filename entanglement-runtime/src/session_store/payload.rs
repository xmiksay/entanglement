//! [`LogPayload`] and its version-tolerant decode (#720). Split out of
//! `session_store.rs` (400-line cap).

use entanglement_core::{InMsg, OutEvent, RETIRED_IN_MSG_KINDS, RETIRED_OUT_EVENT_KINDS};
use serde::{Deserialize, Serialize};

/// Payload of a log record: either an inbound message, an outbound event, a
/// gap tombstone, an adoption pointer, or a frame whose variant has since been
/// retired.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "direction",
    rename_all = "lowercase",
    try_from = "serde_json::Value"
)]
pub enum LogPayload {
    In(InMsg),
    Out(OutEvent),
    /// Tombstone marking that `dropped` records were lost before this point:
    /// the persistence recorder lagged the broadcast (#104), or its
    /// `RecordSink` refused an append (#723).
    /// It is not a real message — a poison marker. The file stays well-formed,
    /// so without it `Session::replay` would silently fold an incomplete
    /// history (e.g. a `ToolCall` missing its `ToolOutput`) into a wrong
    /// `Context`. [`integrity_gap`][super::integrity_gap] detects it so resume
    /// refuses.
    Gap {
        dropped: u64,
    },
    /// Adoption pointer (ADR-0234): the record's session moved under a
    /// session of *this* root's tree while its history stays in root `from`'s
    /// file (the tap routes by spawn-time root). Written by the tap into the
    /// adopting root on `OutEvent::ParentChanged`; `SessionStore::read_tree`
    /// follows it so a resume of this root restores the moved sub-tree. Carries
    /// no state of its own.
    Adopted {
        from: entanglement_core::SessionId,
    },
    /// A frame an older release logged whose `kind` names a variant removed
    /// since (`entanglement_core::RETIRED_IN_MSG_KINDS` /
    /// `RETIRED_OUT_EVENT_KINDS`, e.g. 0.6.0's `set_agent`). Never written by
    /// the tap — only produced on read, so an old log still loads. Unlike
    /// [`Gap`][Self::Gap] it is no hole: the record was decoded and retired on
    /// purpose, carries no state the current engine replays, and every
    /// consumer skips it.
    Retired {
        kind: String,
    },
}

/// The strict shape `LogPayload` had before #720 — the first decode attempt.
#[derive(Deserialize)]
#[serde(tag = "direction", rename_all = "lowercase")]
enum Strict {
    In(InMsg),
    Out(OutEvent),
    Gap { dropped: u64 },
    Adopted { from: entanglement_core::SessionId },
    Retired { kind: String },
}

impl TryFrom<serde_json::Value> for LogPayload {
    type Error = serde_json::Error;

    // WHY decode-then-classify instead of a catch-all variant on the protocol
    // enums: only a frame that fails to decode *and* names a known retired
    // kind is tolerated, so a corrupted or unknown frame still errors (and
    // `read` still reports interior corruption).
    fn try_from(value: serde_json::Value) -> Result<Self, Self::Error> {
        let err = match Strict::deserialize(&value) {
            Ok(Strict::In(m)) => return Ok(Self::In(m)),
            Ok(Strict::Out(e)) => return Ok(Self::Out(e)),
            Ok(Strict::Gap { dropped }) => return Ok(Self::Gap { dropped }),
            Ok(Strict::Adopted { from }) => return Ok(Self::Adopted { from }),
            Ok(Strict::Retired { kind }) => return Ok(Self::Retired { kind }),
            Err(err) => err,
        };
        let retired = match value.get("direction").and_then(|d| d.as_str()) {
            Some("in") => RETIRED_IN_MSG_KINDS,
            Some("out") => RETIRED_OUT_EVENT_KINDS,
            _ => return Err(err),
        };
        match value.get("kind").and_then(|k| k.as_str()) {
            Some(kind) if retired.contains(&kind) => Ok(Self::Retired { kind: kind.into() }),
            _ => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decode(v: serde_json::Value) -> Result<LogPayload, serde_json::Error> {
        serde_json::from_value(v)
    }

    #[test]
    fn retired_frames_decode_as_retired() {
        let set_agent =
            json!({"direction": "in", "kind": "set_agent", "session": "s", "agent": "plan"});
        let bash = json!({"direction": "out", "kind": "bash_changed", "enabled": false});
        for (frame, kind) in [(set_agent, "set_agent"), (bash, "bash_changed")] {
            match decode(frame).expect("retired frame tolerated") {
                LogPayload::Retired { kind: k } => assert_eq!(k, kind),
                other => panic!("expected Retired, got {other:?}"),
            }
        }
    }

    /// A retired tag only counts in its own direction, and an unknown kind is
    /// still an error — the tolerance must not hide corruption.
    #[test]
    fn unknown_or_misdirected_kinds_still_fail() {
        assert!(decode(json!({"direction": "out", "kind": "set_agent"})).is_err());
        assert!(decode(json!({"direction": "in", "kind": "no_such_kind"})).is_err());
        assert!(decode(json!({"direction": "sideways", "kind": "bash_changed"})).is_err());
    }

    #[test]
    fn live_shapes_round_trip_unchanged() {
        for payload in [
            LogPayload::Gap { dropped: 3 },
            LogPayload::Adopted {
                from: entanglement_core::SessionId::new("p"),
            },
            LogPayload::Retired {
                kind: "set_agent".into(),
            },
            LogPayload::In(InMsg::Stop {
                session: entanglement_core::SessionId::new("s"),
            }),
        ] {
            let text = serde_json::to_string(&payload).unwrap();
            let back: LogPayload = serde_json::from_str(&text).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), text);
        }
    }
}
