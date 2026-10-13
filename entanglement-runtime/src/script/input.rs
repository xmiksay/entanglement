//! The `rhai` tool's input (ADR-0228): exactly one of `script` (inline
//! source) or `path` (a predefined script the runtime reads — the model never
//! does), an optional `args` object every run sees as a constant, and the
//! `timeout`/`background` knobs. Validated before the launch gate, so an
//! ill-formed call is declined with the fix spelled out instead of reaching
//! an approval card.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// The largest script file `path` may name — the engine's own string cap,
/// so a file is never larger than a script the model could have inlined.
const MAX_SCRIPT_BYTES: u64 = super::MAX_STRING_SIZE as u64;

/// Where a run's source comes from.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Source {
    Inline(String),
    /// As sent (after the workdir rewrite): root-relative or absolute.
    Path(String),
}

/// A validated `rhai` call.
#[derive(Debug)]
pub(super) struct ScriptInput {
    pub source: Source,
    pub args: Map<String, Value>,
    pub timeout: Option<u64>,
    pub background: bool,
}

const USAGE: &str = r#"pass {"script": "<rhai source>"} or {"path": "scripts/x.rhai"}, either with an optional "args": {"name": value, ...} map"#;

/// Parse and validate `input`; `Err` is the whole decline text.
pub(super) fn parse(input: &str) -> Result<ScriptInput, String> {
    let decline = |why: &str| format!("rhai: invalid input — {why}; {USAGE}");
    let value: Value = serde_json::from_str(input).map_err(|e| decline(&e.to_string()))?;
    let Value::Object(mut obj) = value else {
        return Err(decline("the input must be a JSON object"));
    };
    let mut string = |key: &str| match obj.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(decline(&format!("`{key}` must be a string"))),
    };
    let source = match (string("script")?, string("path")?) {
        (Some(script), None) => Source::Inline(script),
        (None, Some(path)) if path.trim().is_empty() => {
            return Err(decline("`path` must not be empty"));
        }
        (None, Some(path)) if !crate::rhai_trust::is_script(Path::new(&path)) => {
            return Err(decline("`path` must name a `.rhai` script file"));
        }
        (None, Some(path)) => Source::Path(path),
        (Some(_), Some(_)) => {
            return Err(decline(
                "pass exactly one of `script` (inline source) or `path` (a script file), not both",
            ));
        }
        (None, None) => {
            return Err(decline(
                "pass exactly one of `script` (inline source) or `path` (a script file)",
            ));
        }
    };
    let args = match obj.remove("args") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(args)) => args,
        Some(_) => {
            return Err(decline(
                "`args` must be an object of named values, e.g. {\"n\": 3}",
            ))
        }
    };
    let timeout = match obj.remove("timeout") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .ok_or_else(|| decline("`timeout` must be a whole number of seconds"))?,
        ),
    };
    let background = match obj.remove("background") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => b,
        Some(_) => return Err(decline("`background` must be true or false")),
    };
    // A misspelled key (`arguments`, `arg`) would otherwise vanish and the
    // script run without the values the model meant to pass.
    if let Some(key) = obj.keys().next() {
        return Err(decline(&format!("unknown key `{key}`")));
    }
    Ok(ScriptInput {
        source,
        args,
        timeout,
        background,
    })
}

/// A script file as read: its canonical path (what trust is judged on) and
/// the text read from that very file.
#[derive(Debug)]
pub(super) struct ScriptFile {
    pub canon: PathBuf,
    pub text: String,
}

/// Read the script `path` resolves to (`abs`, already resolved by the
/// caller's containment/escape-root check), off the async runtime. `Err` is
/// the reply text.
pub(super) async fn read_script(path: &str, abs: PathBuf) -> Result<ScriptFile, String> {
    let shown = format!("{path:?}");
    tokio::task::spawn_blocking(move || read_capped(&shown, &abs))
        .await
        .unwrap_or_else(|e| Err(format!("rhai: reading the script failed: {e}")))
}

/// Canonicalize once and read from the canonical file, refusing when the
/// file opened is not the one the canonical path named a moment earlier —
/// so trust (judged on `canon`) and the text can never come from two files.
/// On Linux `canon` is then re-derived from the open descriptor itself
/// (`/proc/self/fd`), closing the window in which an intermediate directory
/// is swapped for a symlink between the canonicalize and the open (the
/// `(dev, ino)` check alone only covers the final component). Elsewhere, or
/// without `/proc`, that residual window remains: the canonical path and the
/// inode are both checked, but an intermediate swap racing both is not seen.
/// Only a `.rhai` target is read — a `x.rhai` symlink to `x.txt` is refused.
/// Opened non-blocking and without following a final symlink: a FIFO would
/// otherwise block the open forever, and the `is_file` check rejects it.
/// The cap is enforced on the bytes actually read, not the metadata length:
/// a file can grow between the two, and `/proc`-style files report zero.
fn read_capped(shown: &str, abs: &Path) -> Result<ScriptFile, String> {
    let cannot = |e: std::io::Error| format!("rhai: cannot read {shown}: {e}");
    let canon = abs.canonicalize().map_err(cannot)?;
    let named = std::fs::metadata(&canon).map_err(cannot)?;
    let file = open_nonblocking(&canon).map_err(cannot)?;
    let opened = file.metadata().map_err(cannot)?;
    if !opened.is_file() {
        return Err(format!("rhai: {shown} is not a file"));
    }
    if !same_file(&named, &opened) {
        return Err(format!(
            "rhai: {shown} changed while it was being opened — refusing to run it"
        ));
    }
    let canon = opened_path(&file).unwrap_or(canon);
    // The resolved target is not echoed: for an out-of-root path this text
    // reaches the card before approval.
    if !crate::rhai_trust::is_script(&canon) {
        return Err(format!(
            "rhai: {shown} does not resolve to a `.rhai` script file"
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_SCRIPT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(cannot)?;
    if bytes.len() as u64 > MAX_SCRIPT_BYTES {
        return Err(format!(
            "rhai: {shown} is over the {MAX_SCRIPT_BYTES}-byte script cap"
        ));
    }
    let text = String::from_utf8(bytes).map_err(|_| format!("rhai: {shown} is not UTF-8 text"))?;
    Ok(ScriptFile { canon, text })
}

#[cfg(unix)]
fn open_nonblocking(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_nonblocking(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// The path the kernel holds for an open file — what was really opened.
#[cfg(target_os = "linux")]
fn opened_path(file: &std::fs::File) -> Option<PathBuf> {
    use std::os::fd::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()
}

#[cfg(not(target_os = "linux"))]
fn opened_path(_file: &std::fs::File) -> Option<PathBuf> {
    None
}

#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    (a.dev(), a.ino()) == (b.dev(), b.ino())
}

#[cfg(not(unix))]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.len() == b.len() && a.modified().ok() == b.modified().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(input: &str) -> String {
        parse(input).unwrap_err()
    }

    #[test]
    fn script_or_path_with_args() {
        let p = parse(r#"{"script":"1","args":{"n":3}}"#).unwrap();
        assert_eq!(p.source, Source::Inline("1".into()));
        assert_eq!(p.args["n"], 3);
        let p = parse(r#"{"path":"s/x.rhai","background":true,"timeout":9}"#).unwrap();
        assert_eq!(p.source, Source::Path("s/x.rhai".into()));
        assert!(p.args.is_empty() && p.background);
        assert_eq!(p.timeout, Some(9));
    }

    #[test]
    fn xor_and_types_are_declined_with_the_fix() {
        assert!(err(r#"{"script":"1","path":"x"}"#).contains("not both"));
        assert!(err(r#"{}"#).contains("exactly one of"));
        assert!(err(r#"{"path":"  "}"#).contains("must not be empty"));
        assert!(err(r#"{"script":1}"#).contains("`script` must be a string"));
        assert!(err(r#"{"script":"1","args":[1]}"#).contains("`args` must be an object"));
        assert!(err(r#"{"script":"1","timeout":"5"}"#).contains("`timeout`"));
        assert!(err(r#"{"script":"1","background":"yes"}"#).contains("`background`"));
        assert!(err("[]").contains("JSON object"));
        for not_script in ["notes.txt", "scripts/x", "x.rhai.txt", "dir.rhai/x"] {
            let input = serde_json::json!({ "path": not_script }).to_string();
            assert!(err(&input).contains("`.rhai` script file"), "{not_script}");
        }
        assert!(parse(r#"{"path":"S/X.RHAI"}"#).is_ok(), "any case");
        assert!(err("{").contains(USAGE));
    }

    #[test]
    fn unknown_keys_are_declined_with_the_usage() {
        let e = err(r#"{"path":"x.rhai","arguments":{"n":1}}"#);
        assert!(
            e.contains("unknown key `arguments`") && e.contains(USAGE),
            "{e}"
        );
        assert!(err(r#"{"script":"1","arg":1}"#).contains("unknown key `arg`"));
    }

    #[test]
    fn read_capped_refuses_a_directory_an_oversized_and_a_binary_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_capped("d", dir.path())
            .unwrap_err()
            .contains("not a file"));
        let big = dir.path().join("big.rhai");
        std::fs::write(&big, "x".repeat(MAX_SCRIPT_BYTES as usize + 1)).unwrap();
        assert!(read_capped("big", &big).unwrap_err().contains("cap"));
        let exact = dir.path().join("exact.rhai");
        std::fs::write(&exact, "x".repeat(MAX_SCRIPT_BYTES as usize)).unwrap();
        let read = read_capped("exact", &exact).unwrap();
        assert_eq!(read.canon, exact.canonicalize().unwrap());
        let txt = dir.path().join("notes.txt");
        std::fs::write(&txt, "1").unwrap();
        assert!(read_capped("txt", &txt)
            .unwrap_err()
            .contains("does not resolve to a `.rhai`"));
        let upper = dir.path().join("UP.RHAI");
        std::fs::write(&upper, "1").unwrap();
        assert_eq!(read_capped("up", &upper).unwrap().text, "1");
        let bin = dir.path().join("bin.rhai");
        std::fs::write(&bin, [0xff, 0xfe]).unwrap();
        assert!(read_capped("bin", &bin).unwrap_err().contains("UTF-8"));
        assert!(read_capped("nope", &dir.path().join("nope"))
            .unwrap_err()
            .contains("cannot read"));
    }

    #[tokio::test]
    async fn read_script_reads_off_the_runtime_and_quotes_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("ok.rhai");
        std::fs::write(&ok, "40 + 2").unwrap();
        assert_eq!(read_script("ok.rhai", ok).await.unwrap().text, "40 + 2");
        let e = read_script("a\nb", dir.path().join("missing"))
            .await
            .unwrap_err();
        assert!(e.contains("\"a\\nb\""), "{e}");
    }

    /// A FIFO is refused, never opened in a way that blocks on a writer.
    #[cfg(unix)]
    #[test]
    fn read_capped_refuses_a_fifo_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe.rhai");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
        assert!(read_capped("pipe", &fifo)
            .unwrap_err()
            .contains("not a file"));
    }

    /// The canonical path is the symlink's target: trust is judged on it.
    #[cfg(unix)]
    #[test]
    fn read_capped_reports_the_canonical_target_of_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.rhai");
        std::fs::write(&real, "1").unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("link.rhai")).unwrap();
        let read = read_capped("link", &dir.path().join("link.rhai")).unwrap();
        assert_eq!(read.canon, real.canonicalize().unwrap());
        assert_eq!(read.text, "1");
    }

    /// Trust needs a script: a `.rhai` name pointing at a `.txt` is refused,
    /// judged on the target, not the name the model passed.
    #[cfg(unix)]
    #[test]
    fn read_capped_refuses_a_rhai_symlink_to_a_non_script() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "1").unwrap();
        std::os::unix::fs::symlink(dir.path().join("a.txt"), dir.path().join("a.rhai")).unwrap();
        let e = read_capped("a", &dir.path().join("a.rhai")).unwrap_err();
        assert!(e.contains("does not resolve to a `.rhai`"), "{e}");
        assert!(!e.contains("a.txt"), "target not disclosed: {e}");
    }

    /// On Linux the canonical path comes from the descriptor actually read.
    #[cfg(target_os = "linux")]
    #[test]
    fn opened_path_is_the_descriptor_s_own_path() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.rhai");
        std::fs::write(&real, "1").unwrap();
        let file = std::fs::File::open(&real).unwrap();
        assert_eq!(opened_path(&file), Some(real.canonicalize().unwrap()));
    }
}
