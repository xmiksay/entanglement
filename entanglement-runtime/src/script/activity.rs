//! Live reports of a script's nested calls (#821, ADR-0233): one
//! [`OutEvent::NestedCall`] per binding call for the first
//! [`NESTED_CALL_EVENT_CAP`] calls of a run, then a single
//! [`OutEvent::NestedCallSummary`] at the end counting the rest. Live only —
//! emitted through `Holly::emit_live`, never persisted or replayed.

use std::time::Instant;

use entanglement_core::{Holly, OutEvent, SessionId};

use crate::permission;

/// How many nested calls of one script run are reported individually. A
/// script can loop over thousands of bindings; past this, per-call events
/// would flood every head and the narrator, so the rest are only counted.
pub(crate) const NESTED_CALL_EVENT_CAP: u64 = 50;

/// Character budget of a call's [`args_summary`].
const ARGS_SUMMARY_CHARS: usize = 80;

/// The pure cap/summary arithmetic, apart from emission.
#[derive(Debug, Default)]
struct Tally {
    calls: u64,
    suppressed: u64,
    errors: u64,
}

impl Tally {
    /// Count one starting call. `Some(index)` (1-based) when it is reported
    /// on its own, `None` when it falls past the cap and is only counted.
    fn next(&mut self) -> Option<u64> {
        self.calls += 1;
        if self.calls <= NESTED_CALL_EVENT_CAP {
            return Some(self.calls);
        }
        self.suppressed += 1;
        None
    }

    /// A counted-only call failed.
    fn fail_suppressed(&mut self) {
        self.errors += 1;
    }

    /// `(suppressed, errors among them)`, if any call was held back.
    fn summary(&self) -> Option<(u64, u64)> {
        (self.suppressed > 0).then_some((self.suppressed, self.errors))
    }
}

/// The call currently on the bridge.
struct InFlight {
    /// `Some((index, tool, args summary))` when it is reported on its own.
    report: Option<(u64, String, String)>,
    started: Instant,
}

/// One script run's reporter, owned by the bridge's service loop. The
/// closing summary is emitted on **drop**, not by an explicit call: a `Stop`
/// aborts the service task mid-await, and only `Drop` still runs then.
pub(super) struct NestedCalls {
    holly: Holly,
    session: SessionId,
    parent: String,
    tally: Tally,
    in_flight: Option<InFlight>,
}

impl NestedCalls {
    pub(super) fn new(holly: Holly, session: SessionId, parent: &str) -> Self {
        Self {
            holly,
            session,
            parent: parent.to_string(),
            tally: Tally::default(),
            in_flight: None,
        }
    }

    /// Report on `session` from now on: the run's background script
    /// followed its session's compaction (ADR-0241), so its live activity
    /// shows on the view that continues the work.
    pub(super) fn rehome(&mut self, session: SessionId) {
        self.session = session;
    }

    /// A binding call is about to run. The args summary is computed here,
    /// before the input moves into the dispatcher, and only for a call that
    /// will be reported — a large `write` past the cap is never copied.
    pub(super) fn begin(&mut self, tool: &str, input: &str) {
        let report = self
            .tally
            .next()
            .map(|index| (index, tool.to_string(), args_summary(tool, input)));
        self.in_flight = Some(InFlight {
            report,
            started: Instant::now(),
        });
    }

    /// The call begun last finished.
    pub(super) fn end(&mut self, is_error: bool) {
        let Some(call) = self.in_flight.take() else {
            return;
        };
        let Some((index, tool, args)) = call.report else {
            if is_error {
                self.tally.fail_suppressed();
            }
            return;
        };
        self.holly.emit_live(OutEvent::NestedCall {
            session: self.session.clone(),
            parent_request_id: self.parent.clone(),
            index,
            tool,
            args,
            duration_ms: u64::try_from(call.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            is_error,
        });
    }
}

impl Drop for NestedCalls {
    /// The run is over — finished, or aborted by a `Stop`. A call still on
    /// the bridge was cut off, so it is reported as failed; then the calls
    /// held back past the cap, if any, get their one summary.
    fn drop(&mut self) {
        self.end(true);
        if let Some((suppressed, errors)) = self.tally.summary() {
            self.holly.emit_live(OutEvent::NestedCallSummary {
                session: self.session.clone(),
                parent_request_id: std::mem::take(&mut self.parent),
                suppressed,
                errors,
            });
        }
    }
}

/// A one-line, bounded summary of a call's input that never carries a
/// credential. `bash`/`call` show only the program's name — the rest of a
/// command line is where a `curl -H 'Authorization: …'` rides, and under
/// Allow/`auto`/Run all no card ever showed it. Any other tool shows its
/// permission-graded argument (a path, `{METHOD} {url}`) with every URL's
/// userinfo, query and fragment cut; a tool with none, the input's top-level
/// key names only. `http_auth` credentials never reach a script's input at
/// all — the `http` tool adds them at send time.
pub(crate) fn args_summary(tool: &str, input: &str) -> String {
    let text = match tool {
        "bash" | "call" => program_name(tool, input),
        _ => match permission::permission_arg(tool, input) {
            Some(arg) => strip_url_secrets(&arg),
            None => key_names(input),
        },
    };
    bound(&text)
}

/// The basename of the program a `bash`/`call` input runs, plus ` …` when
/// anything follows it. `bash`'s leading `NAME=value` assignments are
/// skipped, never shown — a value is exactly what must not leak.
fn program_name(tool: &str, input: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(input) else {
        return String::new();
    };
    let command = value.get("command").and_then(|c| c.as_str()).unwrap_or("");
    let mut words = command.split_whitespace().skip_while(|w| w.contains('='));
    let Some(program) = words.next() else {
        return String::new();
    };
    let has_args = tool == "call"
        && value
            .get("args")
            .and_then(|a| a.as_array())
            .is_some_and(|a| !a.is_empty());
    let more = words.next().is_some() || has_args;
    let program = program.trim_matches(['"', '\'']);
    let name = program.rsplit('/').next().unwrap_or(program);
    if more {
        format!("{name} …")
    } else {
        name.to_string()
    }
}

/// The top-level key names of a JSON object input, comma-joined.
fn key_names(input: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect::<Vec<_>>().join(", "),
        _ => String::new(),
    }
}

/// Drop the userinfo, query and fragment of every URL-looking word in
/// `text` (an `http` summary is `{METHOD} {url}`) — where tokens ride.
fn strip_url_secrets(text: &str) -> String {
    text.split(' ')
        .map(|word| {
            let Some((scheme, rest)) = word.split_once("://") else {
                return word.to_string();
            };
            let rest = rest.split(['?', '#']).next().unwrap_or("");
            let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
            let host = authority.rsplit('@').next().unwrap_or(authority);
            format!("{scheme}://{host}{path}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whitespace collapsed to single spaces, capped at [`ARGS_SUMMARY_CHARS`].
fn bound(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= ARGS_SUMMARY_CHARS {
        return line;
    }
    let mut cut: String = line.chars().take(ARGS_SUMMARY_CHARS - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `calls` calls through a tally, every `error_every`-th failing;
    /// returns `(reported indexes, summary)`.
    fn run(calls: u64, error_every: u64) -> (Vec<u64>, Option<(u64, u64)>) {
        let mut tally = Tally::default();
        let mut shown = Vec::new();
        for n in 1..=calls {
            match tally.next() {
                Some(index) => shown.push(index),
                None if n % error_every == 0 => tally.fail_suppressed(),
                None => {}
            }
        }
        (shown, tally.summary())
    }

    #[test]
    fn three_calls_are_three_reports_and_no_summary() {
        let (shown, summary) = run(3, 2);
        assert_eq!(shown, [1, 2, 3]);
        assert_eq!(summary, None);
    }

    #[test]
    fn sixty_calls_are_fifty_reports_and_a_summary_of_the_rest() {
        // Calls 51..=60 are held back; of those, 51/54/57/60 fail.
        let (shown, summary) = run(60, 3);
        assert_eq!(shown, (1..=50).collect::<Vec<_>>());
        assert_eq!(summary, Some((10, 4)));
    }

    #[test]
    fn exactly_the_cap_needs_no_summary() {
        let (shown, summary) = run(NESTED_CALL_EVENT_CAP, 1);
        assert_eq!(shown.len() as u64, NESTED_CALL_EVENT_CAP);
        assert_eq!(summary, None);
    }

    #[test]
    fn a_summary_shows_the_graded_argument() {
        assert_eq!(
            args_summary("read", r#"{"path":"src/main.rs"}"#),
            "src/main.rs"
        );
    }

    #[test]
    fn an_exec_summary_is_the_program_name_only() {
        let bearer = serde_json::json!({
            "command": "curl -H 'Authorization: Bearer s3cret' https://x.example/?token=t"
        })
        .to_string();
        assert_eq!(args_summary("bash", &bearer), "curl …");
        assert_eq!(
            args_summary("bash", r#"{"command":"TOKEN=s3cret /usr/bin/make"}"#),
            "make"
        );
        assert_eq!(
            args_summary(
                "call",
                r#"{"command":"/bin/cargo","args":["test"],"timeout":5}"#
            ),
            "cargo …"
        );
        assert_eq!(args_summary("call", r#"{"command":"ls","args":[]}"#), "ls");
    }

    #[test]
    fn a_call_with_a_multi_word_command_and_no_args_still_marks_more() {
        assert_eq!(
            args_summary("call", r#"{"command":"git push origin","args":[]}"#),
            "git …"
        );
    }

    #[test]
    fn a_quoted_program_is_unquoted_before_its_basename() {
        assert_eq!(
            args_summary("bash", r#"{"command":"'/usr/bin/curl' -s"}"#),
            "curl …"
        );
        assert_eq!(args_summary("bash", r#"{"command":"\"/bin/ls\""}"#), "ls");
    }

    #[test]
    fn a_url_in_any_tools_argument_is_stripped() {
        let summary = args_summary("read", r#"{"path":"https://me:pw@h.example/a?token=t#f"}"#);
        assert_eq!(summary, "https://h.example/a");
    }

    #[test]
    fn a_url_summary_drops_userinfo_query_and_fragment() {
        let input =
            r#"{"method":"get","url":"https://me:pw@api.example.com/v1/x?token=s3cret#frag"}"#;
        let summary = args_summary("http", input);
        assert_eq!(summary, "GET https://api.example.com/v1/x");
        assert!(!summary.contains("s3cret") && !summary.contains("pw"));
    }

    #[test]
    fn an_unknown_tool_summary_lists_keys_never_values() {
        let summary = args_summary("mcp__gh__search", r#"{"query":"secret-ish","limit":5}"#);
        assert_eq!(summary, "limit, query");
        assert_eq!(args_summary("mcp__gh__search", "not json"), "");
    }

    #[test]
    fn a_summary_is_one_bounded_line() {
        let path = format!("dir {}\nmore", "x".repeat(200));
        let input = serde_json::json!({ "path": path }).to_string();
        let summary = args_summary("read", &input);
        assert!(!summary.contains('\n'));
        assert_eq!(summary.chars().count(), ARGS_SUMMARY_CHARS);
        assert!(summary.ends_with('…'));
    }
}
