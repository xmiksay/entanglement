//! [`resumable_prefix`]: the embedder-side "cut a loaded log at its first
//! hole" helper (#723), so a DB-backed store stops re-implementing gap
//! truncation.

use super::{LogPayload, LogRecord};

/// Where and why [`resumable_prefix`] cut a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truncation {
    /// The row at `index` is a [`LogPayload::Gap`] tombstone for `dropped`
    /// lost records.
    Gap { index: usize, dropped: u64 },
    /// The row at `index` could not be decoded by the embedder.
    Corrupt { index: usize },
}

impl Truncation {
    /// Index (in the input rows) of the first row not in the prefix.
    pub fn index(&self) -> usize {
        match *self {
            Self::Gap { index, .. } | Self::Corrupt { index } => index,
        }
    }
}

/// The part of a log that is safe to replay.
#[derive(Debug, Clone)]
pub struct ResumablePrefix {
    /// Every record before the first hole, in order.
    pub records: Vec<LogRecord>,
    /// `None` when the whole log was intact.
    pub truncation: Option<Truncation>,
}

impl ResumablePrefix {
    pub fn truncated(&self) -> bool {
        self.truncation.is_some()
    }
}

/// Cuts loaded log rows down to the prefix that is safe to resume from.
///
/// `rows` is the log as an embedder loaded it, one item per stored row: `Ok`
/// for a decoded [`LogRecord`], `Err` for a row it could not decode (the
/// error is not inspected — log it before passing it in if you need it).
/// The prefix ends just before the first `Gap` tombstone or undecodable row:
/// everything after a hole was recorded against a history replay can no
/// longer reconstruct, so it is dropped too. A `Retired` record (#720) is no
/// hole — it was decoded and retired on purpose — so it stays in the prefix
/// for `pair_records`/replay to skip like they always do.
///
/// This is the *truncate* policy, for embedders that prefer resuming the
/// intact head over refusing. The in-tree heads keep #104's stricter *refuse*
/// policy ([`check_for_resume`][super::check_for_resume]): both use
/// [`LogPayload::gap_dropped`] to decide what a gap is.
pub fn resumable_prefix<E>(
    rows: impl IntoIterator<Item = Result<LogRecord, E>>,
) -> ResumablePrefix {
    let mut records = Vec::new();
    for (index, row) in rows.into_iter().enumerate() {
        let Ok(record) = row else {
            return ResumablePrefix {
                records,
                truncation: Some(Truncation::Corrupt { index }),
            };
        };
        if let Some(dropped) = record.payload.gap_dropped() {
            return ResumablePrefix {
                records,
                truncation: Some(Truncation::Gap { index, dropped }),
            };
        }
        records.push(record);
    }
    ResumablePrefix {
        records,
        truncation: None,
    }
}

impl LogPayload {
    /// `Some(dropped)` for a [`LogPayload::Gap`] tombstone — the one
    /// definition of "a hole" every integrity check shares.
    pub fn gap_dropped(&self) -> Option<u64> {
        match self {
            Self::Gap { dropped } => Some(*dropped),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::{InMsg, SessionId};

    /// A record whose session id doubles as its label, so a test can name
    /// which rows survived.
    fn prompt(label: &str) -> LogRecord {
        let sid = SessionId::new(label);
        LogRecord::new(sid.clone(), LogPayload::In(InMsg::prompt(sid, "hi")))
    }

    fn texts(prefix: &ResumablePrefix) -> Vec<String> {
        prefix
            .records
            .iter()
            .map(|r| r.session.to_string())
            .collect()
    }

    fn rec(p: LogPayload) -> Result<LogRecord, ()> {
        Ok(LogRecord::new(SessionId::new("root"), p))
    }

    #[test]
    fn intact_log_is_kept_whole() {
        let rows = vec![Ok::<_, ()>(prompt("a")), Ok(prompt("b"))];
        let prefix = resumable_prefix(rows);
        assert!(!prefix.truncated());
        assert_eq!(texts(&prefix), ["a", "b"]);
    }

    #[test]
    fn cuts_at_the_first_gap() {
        let rows = vec![
            Ok(prompt("a")),
            rec(LogPayload::Gap { dropped: 2 }),
            Ok(prompt("b")),
            rec(LogPayload::Gap { dropped: 5 }),
        ];
        let prefix = resumable_prefix(rows);
        assert_eq!(texts(&prefix), ["a"]);
        assert_eq!(
            prefix.truncation,
            Some(Truncation::Gap {
                index: 1,
                dropped: 2
            })
        );
    }

    #[test]
    fn cuts_at_the_first_corrupt_row() {
        let rows = vec![
            Ok(prompt("a")),
            Ok(prompt("b")),
            Err("bad json"),
            Ok(prompt("c")),
        ];
        let prefix = resumable_prefix(rows);
        assert_eq!(texts(&prefix), ["a", "b"]);
        assert_eq!(prefix.truncation, Some(Truncation::Corrupt { index: 2 }));
        assert_eq!(prefix.truncation.map(|t| t.index()), Some(2));
    }

    #[test]
    fn retired_records_are_not_gaps() {
        let rows = vec![
            Ok(prompt("a")),
            rec(LogPayload::Retired {
                kind: "set_agent".into(),
            }),
            Ok(prompt("b")),
        ];
        let prefix = resumable_prefix(rows);
        assert!(!prefix.truncated());
        assert_eq!(prefix.records.len(), 3);
        assert!(matches!(
            prefix.records[1].payload,
            LogPayload::Retired { .. }
        ));
    }

    #[test]
    fn hole_at_the_head_yields_an_empty_prefix() {
        let prefix = resumable_prefix(vec![Err::<LogRecord, _>(()), Ok(prompt("a"))]);
        assert!(prefix.records.is_empty());
        assert_eq!(prefix.truncation, Some(Truncation::Corrupt { index: 0 }));
    }
}
