//! Attention signals (issue #14): ring the terminal bell — and, opt-in, raise a
//! desktop notification — when a session reaches a state the user cares about
//! while the terminal sits in the background: waiting for approval, finished, or
//! errored.
//!
//! Core emits `Status` exactly on a state change, so keying off the three target
//! states *is* keying off the three transitions — no extra edge tracking needed.
//! `Done`/`Error` also arrive as their own `OutEvent` variants; we deliberately
//! signal only on `Status` so a single turn end rings once.

use std::io::Write;

use entanglement_core::{AgentState, OutEvent};

/// The bytes to emit for one attention signal: always the bell, optionally
/// followed by an OSC 9 desktop-notification sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signal {
    bytes: Vec<u8>,
}

impl Signal {
    fn new(state: AgentState, notify: bool) -> Self {
        // BEL — universally understood, and (unlike cursor moves) safe to inject
        // mid-frame without corrupting ratatui's alternate-screen drawing.
        let mut bytes = vec![0x07];
        if notify {
            // OSC 9: `ESC ] 9 ; <text> BEL`. iTerm2/kitty/WezTerm surface it as a
            // desktop notification; terminals that don't recognise it drop the
            // sequence silently.
            let text = format!("skutter: {}", describe(state));
            bytes.extend_from_slice(b"\x1b]9;");
            bytes.extend_from_slice(text.as_bytes());
            bytes.push(0x07);
        }
        Self { bytes }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

fn describe(state: AgentState) -> &'static str {
    match state {
        AgentState::WaitingApproval => "waiting for approval",
        AgentState::WaitingAnswer => "waiting for answer",
        AgentState::Done => "turn complete",
        AgentState::Error => "turn failed",
        AgentState::Idle | AgentState::Thinking => "",
    }
}

/// Decides when to raise an attention signal and writes it to the terminal.
pub struct Attention {
    /// OSC 9 desktop notification, opt-in via `ENTANGLEMENT_TUI_NOTIFY=1`.
    notify: bool,
    /// Terminal focus as last reported by crossterm. `None` until the first
    /// focus event: most terminals never send one, so we default to *not*
    /// suppressing (signal always fires) and only mute while genuinely focused.
    focused: Option<bool>,
}

impl Attention {
    pub fn from_env() -> Self {
        let notify = std::env::var("ENTANGLEMENT_TUI_NOTIFY")
            .map(|v| v == "1")
            .unwrap_or(false);
        Self {
            notify,
            focused: None,
        }
    }

    /// Records the terminal's focus, driven by crossterm `FocusGained`/`FocusLost`.
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = Some(focused);
    }

    /// Inspects an engine event and, on a signal-worthy `Status` transition,
    /// writes the bell (+ optional notification) to the given sink.
    pub fn observe<W: Write>(&mut self, event: &OutEvent, out: &mut W) {
        if let OutEvent::Status { state, .. } = event {
            if let Some(signal) = self.decide(*state) {
                let _ = out.write_all(signal.as_bytes());
                let _ = out.flush();
            }
        }
    }

    /// Pure signal decision: `Some` for a target state we should surface, unless
    /// the terminal is known to be focused. `None` otherwise.
    fn decide(&self, state: AgentState) -> Option<Signal> {
        let worth_signalling = matches!(
            state,
            AgentState::WaitingApproval
                | AgentState::WaitingAnswer
                | AgentState::Done
                | AgentState::Error
        );
        if !worth_signalling || self.focused == Some(true) {
            return None;
        }
        Some(Signal::new(state, self.notify))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::SessionId;

    fn att(notify: bool) -> Attention {
        Attention {
            notify,
            focused: None,
        }
    }

    #[test]
    fn signals_only_on_target_states() {
        let a = att(false);
        for (state, expected) in [
            (AgentState::Idle, false),
            (AgentState::Thinking, false),
            (AgentState::WaitingApproval, true),
            (AgentState::Done, true),
            (AgentState::Error, true),
        ] {
            assert_eq!(a.decide(state).is_some(), expected, "{state:?}");
        }
    }

    #[test]
    fn bell_without_notify_is_just_bel() {
        let a = att(false);
        let sig = a.decide(AgentState::Done).unwrap();
        assert_eq!(sig.as_bytes(), &[0x07]);
    }

    #[test]
    fn notify_appends_osc9_sequence() {
        let a = att(true);
        let sig = a.decide(AgentState::Error).unwrap();
        let bytes = sig.as_bytes();
        assert_eq!(bytes[0], 0x07);
        let tail = &bytes[1..];
        assert!(tail.starts_with(b"\x1b]9;"), "OSC 9 introducer");
        assert_eq!(*tail.last().unwrap(), 0x07, "BEL terminator");
        let text = std::str::from_utf8(&tail[4..tail.len() - 1]).unwrap();
        assert_eq!(text, "skutter: turn failed");
    }

    #[test]
    fn suppressed_only_while_focused() {
        let mut a = att(false);
        a.set_focused(false);
        assert!(a.decide(AgentState::Done).is_some(), "unfocused fires");
        a.set_focused(true);
        assert!(a.decide(AgentState::Done).is_none(), "focused mutes");
    }

    #[test]
    fn observe_writes_bell_for_status_event() {
        let mut a = att(false);
        let mut buf = Vec::new();
        a.observe(
            &OutEvent::Status {
                session: SessionId::new("s1"),
                state: AgentState::WaitingApproval,
            },
            &mut buf,
        );
        assert_eq!(buf, vec![0x07]);
    }

    #[test]
    fn observe_ignores_non_status_events() {
        let mut a = att(false);
        let mut buf = Vec::new();
        a.observe(
            &OutEvent::Done {
                session: SessionId::new("s1"),
                seq: 1,
            },
            &mut buf,
        );
        assert!(buf.is_empty(), "Done variant must not double-ring");
    }
}
