//! Copy text to the system clipboard via an **OSC 52** terminal escape.
//!
//! OSC 52 is the portable, dependency-free path: it works over SSH and inside
//! tmux (with `set -g set-clipboard on`) and is supported by kitty/wezterm/
//! iTerm2/foot/Alacritty — no X11/Wayland client libs (unlike `arboard`). The
//! trade-off is that copy relies on the terminal honoring OSC 52; a terminal
//! that doesn't will silently ignore it. Written straight to the terminal
//! backend (not `println!`) so it rides the same handle as rendering and can't
//! corrupt the alternate screen.

use std::io::Write;

use anyhow::{Context, Result};
use base64::Engine;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Write `text` to the clipboard via OSC 52. The escape is
/// `ESC ] 52 ; c ; <base64(text)> BEL` — selection `c` = the system clipboard.
pub fn copy_osc52(terminal: &mut Term, text: &str) -> Result<()> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let backend = terminal.backend_mut();
    write!(backend, "\x1b]52;c;{encoded}\x07").context("writing OSC 52 clipboard escape")?;
    backend
        .flush()
        .context("flushing OSC 52 clipboard escape")?;
    Ok(())
}

/// The OSC 52 escape sequence for `text` — split out from the terminal write so
/// the encoding is unit-testable.
#[cfg(test)]
fn osc52_sequence(text: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x07")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_wraps_base64_in_the_clipboard_escape() {
        // "hi" → base64 "aGk=", framed as ESC ] 52 ; c ; <b64> BEL.
        assert_eq!(osc52_sequence("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn osc52_encodes_multibyte_text() {
        let seq = osc52_sequence("héllo");
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\x07'));
        let b64 = &seq[7..seq.len() - 1];
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "héllo");
    }
}
