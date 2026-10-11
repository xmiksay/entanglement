//! Clipboard access: copy text via an **OSC 52** terminal escape, and read an
//! image back for paste ([`read_image`], #736).
//!
//! OSC 52 is the portable, dependency-free path: it works over SSH and inside
//! tmux (with `set -g set-clipboard on`) and is supported by kitty/wezterm/
//! iTerm2/foot/Alacritty — no X11/Wayland client libs (unlike `arboard`). The
//! trade-off is that copy relies on the terminal honoring OSC 52; a terminal
//! that doesn't will silently ignore it. Written straight to the terminal
//! backend (not `println!`) so it rides the same handle as rendering and can't
//! corrupt the alternate screen.

use std::io::Write;
use std::time::Duration;

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

/// Image media types the model providers accept inline — the same set `read`
/// recognizes for an image file (#221).
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Read cap for a clipboard tool's stdout, so a pathological clipboard can't
/// balloon memory before the (much smaller) encoded-size cap is applied.
const READ_CAP: u64 = 32 * 1024 * 1024;
const TOOL_TIMEOUT: Duration = Duration::from_secs(3);

/// Read an image off the system clipboard (#736). There is no terminal
/// escape for this (OSC 52 is text-only and its read-back is widely
/// disabled), so it shells out to the platform's own clipboard tool —
/// `wl-paste` (Wayland), `xclip` (X11), `osascript` (macOS) — rather than
/// linking `arboard`'s X11/Wayland client stack into every build. A headless
/// session or a missing tool is an `Err` with an actionable message, never a
/// panic.
pub async fn read_image() -> std::result::Result<Vec<u8>, String> {
    if cfg!(target_os = "macos") {
        let out = run("osascript", &["-e", "the clipboard as «class PNGf»"]).await?;
        return out
            .as_deref()
            .and_then(parse_osascript_png)
            .ok_or_else(|| "no image on the clipboard".to_string());
    }
    let backend =
        clipboard_backend(env_set("WAYLAND_DISPLAY"), env_set("DISPLAY")).ok_or_else(|| {
            "no graphical clipboard in this session (needs Wayland or X11)".to_string()
        })?;
    let types = match backend {
        Backend::Wayland => run("wl-paste", &["--list-types"]).await?,
        Backend::X11 => run("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"]).await?,
    };
    let Some(mime) = pick_image_type(&String::from_utf8_lossy(&types.unwrap_or_default())) else {
        return Err("no image on the clipboard".into());
    };
    let bytes = match backend {
        Backend::Wayland => run("wl-paste", &["--no-newline", "--type", mime]).await?,
        Backend::X11 => run("xclip", &["-selection", "clipboard", "-t", mime, "-o"]).await?,
    };
    match bytes {
        Some(bytes) if !bytes.is_empty() => Ok(bytes),
        _ => Err("no image on the clipboard".into()),
    }
}

#[derive(Debug, PartialEq)]
enum Backend {
    Wayland,
    X11,
}

/// Wayland wins when both are set (XWayland exports `DISPLAY` too, but
/// `xclip` there only sees X clients' selections).
fn clipboard_backend(wayland: bool, x11: bool) -> Option<Backend> {
    match (wayland, x11) {
        (true, _) => Some(Backend::Wayland),
        (false, true) => Some(Backend::X11),
        _ => None,
    }
}

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

/// The first supported image type in a clipboard tool's offered-types list
/// (one MIME type per line).
fn pick_image_type(offered: &str) -> Option<&'static str> {
    let offered: Vec<&str> = offered.lines().map(str::trim).collect();
    IMAGE_TYPES.into_iter().find(|t| offered.contains(t))
}

/// `osascript` prints PNG clipboard data as `«data PNGf89504E47…»`.
fn parse_osascript_png(out: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(out).ok()?.trim();
    let hex = text.strip_prefix("«data PNGf")?.strip_suffix('»')?;
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Run a clipboard tool, returning its stdout on success, `None` on a
/// non-zero exit (the tool's way of saying "nothing of that type"), and an
/// actionable `Err` when the tool isn't installed or hangs.
async fn run(program: &str, args: &[&str]) -> std::result::Result<Option<Vec<u8>>, String> {
    use tokio::io::AsyncReadExt;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                format!("`{program}` not found — install it to paste images")
            }
            _ => format!("running `{program}`: {e}"),
        })?;
    let mut bytes = Vec::new();
    let stdout = child.stdout.take();
    let read = async {
        if let Some(stdout) = stdout {
            stdout.take(READ_CAP).read_to_end(&mut bytes).await?;
        }
        child.wait().await
    };
    match tokio::time::timeout(TOOL_TIMEOUT, read).await {
        Ok(Ok(status)) if status.success() => Ok(Some(bytes)),
        Ok(Ok(_)) => Ok(None),
        Ok(Err(e)) => Err(format!("reading from `{program}`: {e}")),
        Err(_) => Err(format!("`{program}` timed out")),
    }
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
    fn backend_prefers_wayland_and_is_none_when_headless() {
        assert_eq!(clipboard_backend(true, true), Some(Backend::Wayland));
        assert_eq!(clipboard_backend(false, true), Some(Backend::X11));
        assert_eq!(clipboard_backend(false, false), None);
    }

    #[test]
    fn picks_the_first_supported_image_type_offered() {
        let offered = "text/plain\nimage/bmp\nimage/jpeg\nimage/png\n";
        assert_eq!(pick_image_type(offered), Some("image/png"));
        assert_eq!(pick_image_type("image/webp"), Some("image/webp"));
        assert_eq!(pick_image_type("text/plain\nimage/bmp"), None);
    }

    #[test]
    fn parses_osascript_png_hex() {
        assert_eq!(
            parse_osascript_png("«data PNGf89504E47»\n".as_bytes()),
            Some(vec![0x89, 0x50, 0x4e, 0x47])
        );
        assert_eq!(parse_osascript_png(b"missing value"), None);
        assert_eq!(parse_osascript_png("«data PNGf8»".as_bytes()), None);
    }

    #[tokio::test]
    async fn a_missing_clipboard_tool_is_an_actionable_error() {
        let err = run("entanglement-no-such-clipboard-tool", &[])
            .await
            .unwrap_err();
        assert!(err.contains("not found"), "{err}");
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
