//! ADR-0228's explicit denies: a mode deny of `rhai` itself refuses even a
//! trusted path (before the file is touched), and inside a trusted or
//! **Run all** script an overlay deny, a `pre_tool_use` veto and a ceiling
//! deny of a nested tool are each a catchable error. Plus the never-hang
//! posture of a script `path` naming a FIFO.

use std::path::Path;

use entanglement_core::{Permission, PermissionProfile, ToolOverlayEntry};

use super::launch_harness::{fixture, run, table, Launch, Setup};

/// Tries a nested `write`, returning `ran` or the caught refusal.
const TRY_WRITE: &str = r#"let r = "ran"; try { write("no.txt", "x"); } catch (e) { r = e; } r"#;

/// An `Ask` mode with `scripts/` trusted and `scripts/try.rhai` = [`TRY_WRITE`].
fn trusted(root: &Path, launch: Launch) -> Setup {
    std::fs::write(root.join("scripts/try.rhai"), TRY_WRITE).unwrap();
    let mut setup = Setup::new(table(Permission::Ask, &[], &[], 0), launch);
    setup.trusted = vec![root.join("scripts")].into();
    setup
}

const TRUSTED_TRY: &str = r#"{"path":"scripts/try.rhai"}"#;

fn inline_try() -> String {
    serde_json::json!({ "script": TRY_WRITE }).to_string()
}

/// Run the nested-`write` script both trusted and under **Run all**; each
/// output must carry `needle` and the write must not have happened.
async fn assert_nested_write_refused(configure: impl Fn(&mut Setup), needle: &str) {
    for trusted_run in [true, false] {
        let (_tmp, root) = fixture();
        let mut setup = trusted(&root, Launch::RunAll);
        configure(&mut setup);
        let input = if trusted_run {
            TRUSTED_TRY.to_string()
        } else {
            inline_try()
        };
        let seen = run(&root, &[&input], setup).await;
        assert_eq!(
            seen.launch_cards.len(),
            usize::from(!trusted_run),
            "{:?}",
            seen.outputs
        );
        assert!(seen.nested_cards.is_empty(), "{:?}", seen.nested_cards);
        assert!(
            seen.outputs[0].contains(needle),
            "trusted={trusted_run}: {:?}",
            seen.outputs
        );
        assert!(!root.join("no.txt").exists());
    }
}

#[tokio::test]
async fn a_mode_deny_of_rhai_refuses_even_a_trusted_path() {
    let (_tmp, root) = fixture();
    let mut setup = trusted(&root, Launch::RunAll);
    setup.table = table(Permission::Ask, &[], &["rhai"], 0);
    let seen = run(&root, &[TRUSTED_TRY], setup).await;
    assert!(seen.launch_cards.is_empty());
    assert!(
        seen.outputs[0].contains("tool `rhai` denied by mode"),
        "{:?}",
        seen.outputs
    );
}

#[tokio::test]
async fn an_overlay_deny_of_a_nested_tool_wins_under_trust_and_run_all() {
    assert_nested_write_refused(
        |s| s.overlay = vec![ToolOverlayEntry::deny("write")],
        "withdrawn for this session",
    )
    .await;
}

#[tokio::test]
async fn a_nested_pre_tool_use_veto_wins_under_trust_and_run_all() {
    assert_nested_write_refused(
        |s| {
            s.hooks = serde_yaml::from_str(
                "pre_tool_use:\n  - command: \"echo no writes >&2; exit 1\"\n    tools: [write]\n",
            )
            .unwrap()
        },
        "no writes",
    )
    .await;
}

#[tokio::test]
async fn a_ceiling_deny_of_a_nested_tool_wins_under_trust_and_run_all() {
    assert_nested_write_refused(
        // Arg-scoped: a bare `write` ceiling rule also clamps the inline
        // `rhai` launch by capability, before any nested call.
        |s| {
            s.ceiling =
                PermissionProfile::new(Permission::Allow).with("write(no.txt)", Permission::Deny)
        },
        "denied by mode",
    )
    .await;
}

/// The checks that need only the input run first: a hook can still create
/// the very file the launch then reads.
#[tokio::test]
async fn the_file_is_read_only_after_the_pre_tool_use_hook() {
    let (_tmp, root) = fixture();
    let mut setup = trusted(&root, Launch::Reject);
    setup.hooks = serde_yaml::from_str(&format!(
        "pre_tool_use:\n  - command: \"printf 41+1 > {}\"\n    tools: [rhai]\n",
        root.join("scripts/late.rhai").display()
    ))
    .unwrap();
    let seen = run(&root, &[r#"{"path":"scripts/late.rhai"}"#], setup).await;
    assert!(seen.outputs[0].contains("42"), "{:?}", seen.outputs);
}

fn mkfifo(path: &Path) {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
}

/// An inotify watch for `IN_OPEN` on one path: whether anything opened it
/// since the watch was set — deterministic, unlike racing a FIFO writer.
#[cfg(target_os = "linux")]
struct OpenWatch(std::os::fd::OwnedFd);

#[cfg(target_os = "linux")]
impl OpenWatch {
    fn new(path: &Path) -> Self {
        use std::os::fd::FromRawFd;
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0, "inotify_init1");
        // SAFETY: a fresh descriptor this watch alone owns.
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        use std::os::fd::AsRawFd;
        let wd = unsafe { libc::inotify_add_watch(fd.as_raw_fd(), c.as_ptr(), libc::IN_OPEN) };
        assert!(wd >= 0, "inotify_add_watch");
        OpenWatch(fd)
    }

    /// `true` when an open event is queued; `EAGAIN` means none.
    fn opened(&self) -> bool {
        use std::os::fd::AsRawFd;
        let mut buf = [0u8; 4096];
        let n = unsafe { libc::read(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        n > 0
    }
}

/// A FIFO in a trusted folder: the launch errors instead of blocking on the
/// open forever.
#[tokio::test]
async fn a_fifo_path_errors_instead_of_hanging() {
    let (_tmp, root) = fixture();
    let setup = trusted(&root, Launch::RunAll);
    mkfifo(&root.join("scripts/pipe.rhai"));
    let seen = run(&root, &[r#"{"path":"scripts/pipe.rhai"}"#], setup).await;
    assert!(seen.outputs[0].contains("not a file"), "{:?}", seen.outputs);
}

/// A mode-denied launch never opens its file (a FIFO, so even an open
/// attempt would show).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_mode_denied_launch_never_opens_the_file() {
    let (_tmp, root) = fixture();
    let fifo = root.join("scripts/pipe.rhai");
    mkfifo(&fifo);
    let mut setup = trusted(&root, Launch::RunAll);
    setup.table = table(Permission::Ask, &[], &["rhai"], 0);
    let watch = OpenWatch::new(&fifo);
    let seen = run(&root, &[r#"{"path":"scripts/pipe.rhai"}"#], setup).await;
    assert!(
        seen.outputs[0].contains("denied by mode"),
        "{:?}",
        seen.outputs
    );
    assert!(!watch.opened(), "the launch opened it");

    // The control: an allowed launch does open it.
    let setup = trusted(&root, Launch::RunAll);
    let watch = OpenWatch::new(&fifo);
    let seen = run(&root, &[r#"{"path":"scripts/pipe.rhai"}"#], setup).await;
    assert!(seen.outputs[0].contains("not a file"), "{:?}", seen.outputs);
    assert!(watch.opened(), "the launch never opened it");
}
