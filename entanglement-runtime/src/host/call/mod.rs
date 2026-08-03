//! `call` — direct process execution (argv, **no shell**) with auto-tailed
//! output. Complements `bash` (ADR-0009): what the model sends as `command` +
//! `args` execs verbatim — no `sh -c`, so no pipes, globbing, `$VAR` expansion,
//! or metacharacter injection. A fixed argv is auditable, which is why a profile
//! may reasonably `Allow` `call` while keeping `bash` at `Ask`/`Deny`. Runs
//! unsandboxed with the engine's full privileges, but — unlike `bash` — is
//! registered unconditionally, independent of `ENTANGLEMENT_ENABLE_BASH`
//! (ADR-0093, supersedes ADR-0010 §3/ADR-0045 §3 for `call`); per-profile
//! permission (`Allow`/`Ask`/`Deny`) is the actual dispatch gate.
//!
//! `input_file`/`output_file` (ADR-0092, #381) give a call a durable trace:
//! `input_file` is read before spawn and piped to the child's stdin (no
//! `input_file` → stdin is explicitly closed, not inherited from the engine);
//! `output_file` gets the full untruncated stdout, with a `<output_file>.stderr`
//! sibling always written alongside. With no `output_file`, a result that
//! overflows its tail/byte cap is retained in memory instead (#608, ADR-0161
//! §7, [`crate::retained_output::RetainedOutputRegistry`]) and paged via
//! `poll` — no scratch file, no absolute path spent in the result.
//!
//! **A truncated result keeps its handle either way** (ADR-0161 §7): with no
//! `output_file` the handle pages the retained text; with an explicit
//! `output_file` the file already holds the full text, so the handle just
//! lets `poll` report that path back.
//!
//! `background: true` (#606, ADR-0161 §1) spawns detached into the shared
//! [`JobRegistry`] instead — the same mechanism `bash` uses — and returns a job
//! id to `poll` immediately rather than blocking. It is refused alongside
//! `input_file`/`output_file`: a backgrounded job's output streams through
//! `poll`, not a file artifact, and `input_file`'s stdin-feed has no running
//! wait to pipe into once the call has already returned.
//!
//! Like `bash`, an opt-in bubblewrap confinement layer is available
//! (ADR-0104, [`SandboxPolicy`]).

mod background;
mod foreground;
mod format;
mod output;
mod validate;

use super::jobs::JobRegistry;
use super::sandbox::SandboxPolicy;
use crate::policy::SandboxResolver;
use crate::retained_output::RetainedOutputRegistry;
use crate::tools::Tool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use entanglement_core::{ContentPart, SessionId};
use serde::Deserialize;
use std::borrow::Cow;
use std::sync::Arc;

const MAX_CALL_TIMEOUT_SECONDS: u64 = 600;

pub struct CallTool {
    root: std::path::PathBuf,
    /// Env vars scrubbed from the child before spawn — the provider API keys
    /// (`ZAI_API_KEY`, …) so a model-authored binary can't read the engine's
    /// credentials (#164). The no-shell design doesn't help here: a plain
    /// `env`/`printenv` still inherits them. Empty by default; wired from the
    /// catalog.
    secret_env: Vec<String>,
    /// Confinement resolver (#479, ADR-0104 amendment) — see `BashTool`'s
    /// identical field for the fixed-policy-is-its-own-resolver rationale.
    /// Defaults to [`SandboxPolicy::none()`] — unsandboxed, unchanged from
    /// before either existed.
    sandbox_resolver: Arc<dyn SandboxResolver>,
    /// Approval-gated out-of-root `workdir` (ADR-0109).
    extra_roots: Option<std::sync::Arc<crate::extra_roots::ExtraRootStore>>,
    /// Live bash-registration handle (#554) — lets the shape-check error in
    /// [`validate::check_no_shell`] tell a model whether the `bash` tool is
    /// actually reachable right now (startup `ENTANGLEMENT_ENABLE_BASH=1` or a
    /// live `/bash on`) instead of unconditionally pointing at a tool that,
    /// out of the box, doesn't exist. `None` (the standalone/test constructor
    /// path) is treated as "unavailable".
    bash_status: Option<Arc<crate::bash_live::LiveBashState>>,
    /// Background-job registry shared with `bash` and `poll` (#606). A private
    /// per-tool default keeps standalone/TUI construction working; the head
    /// wires the shared instance via [`CallTool::with_jobs`] so polls reach the
    /// jobs this tool spawned.
    jobs: JobRegistry,
    /// Retained-output registry shared with `poll` (#608): a truncated
    /// blocking result's full text (or, with an explicit `output_file`, just
    /// the path) is registered here under a fresh handle. A private per-tool
    /// default keeps standalone/TUI construction working; the head wires the
    /// shared instance via [`CallTool::with_retained_output`].
    retained: RetainedOutputRegistry,
}

impl CallTool {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self {
            root,
            secret_env: Vec::new(),
            sandbox_resolver: Arc::new(SandboxPolicy::none()),
            extra_roots: None,
            bash_status: None,
            jobs: JobRegistry::new(),
            retained: RetainedOutputRegistry::new(),
        }
    }

    /// Permit an approved out-of-root `workdir` (ADR-0109).
    pub fn with_extra_roots(
        mut self,
        extra: std::sync::Arc<crate::extra_roots::ExtraRootStore>,
    ) -> Self {
        self.extra_roots = Some(extra);
        self
    }

    /// Share `jobs` with the runtime-owned `poll` tool so background jobs this
    /// tool spawns are pollable (#606).
    pub fn with_jobs(mut self, jobs: JobRegistry) -> Self {
        self.jobs = jobs;
        self
    }

    /// Share `retained` with the runtime-owned `poll` tool so a truncated
    /// blocking result's handle is pollable (#608).
    pub fn with_retained_output(mut self, retained: RetainedOutputRegistry) -> Self {
        self.retained = retained;
        self
    }

    /// Scrub `vars` from the spawned command's environment (provider API keys).
    pub fn with_secret_env(mut self, vars: Vec<String>) -> Self {
        self.secret_env = vars;
        self
    }

    /// Confine every spawned command under `policy` (ADR-0104), regardless of
    /// session/profile.
    pub fn with_sandbox(mut self, policy: SandboxPolicy) -> Self {
        self.sandbox_resolver = Arc::new(policy);
        self
    }

    /// Resolve the confinement policy per session/profile (#479, ADR-0104
    /// amendment) instead of a single fixed [`SandboxPolicy`].
    pub fn with_sandbox_resolver(mut self, resolver: Arc<dyn SandboxResolver>) -> Self {
        self.sandbox_resolver = resolver;
        self
    }

    /// Read bash registration live from `status` (#554) so the shape-check
    /// error can point at `bash` only when it is actually reachable.
    pub fn with_bash_status(mut self, status: Arc<crate::bash_live::LiveBashState>) -> Self {
        self.bash_status = Some(status);
        self
    }
}

#[derive(Deserialize)]
struct CallInput {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default = "default_tail")]
    tail: u32,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    input_file: Option<String>,
    #[serde(default)]
    output_file: Option<String>,
    /// Optional per-call working directory, resolved under the tool root.
    #[serde(default)]
    workdir: Option<String>,
    /// Spawn detached and return a job id to poll via `poll` instead of
    /// blocking (#606, ADR-0161 §1). Refused alongside `input_file`/
    /// `output_file` — see the module doc for why.
    #[serde(default)]
    background: bool,
}

fn default_tail() -> u32 {
    super::DEFAULT_TAIL
}

#[async_trait]
impl Tool for CallTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("call")
    }
    fn description(&self) -> &str {
        "Execute a binary directly (argv, NO shell) in the working directory \
         (or `workdir`): `command` + `args` are passed verbatim to exec — no \
         `sh -c`, so pipes, globbing, `$VAR` expansion, and metacharacters are \
         NOT interpreted. Prefer this over `bash` for a fixed command. Returns \
         `[exit N]`, stdout, and a `[stderr]` block, each tailed to its last \
         `tail` lines (default 30 — command value concentrates at the end; \
         `tail=0` for full output, still byte-capped). A result that overflows \
         the cap keeps a handle either way: with no `output_file`, poll it to \
         page the retained remainder; with an `output_file`, poll it to be \
         reminded of that path. Pass `background=true` to start a long process \
         (build, dev server) detached and get a job id — poll it with `poll`; \
         refused together with `input_file`/`output_file`."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Binary to execute (looked up on PATH). Run \
                        directly, not through a shell."
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Arguments passed verbatim as argv — no shell \
                        interpretation. Default []."
                },
                "tail": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Keep only the last N lines of each stream \
                        (default 30). Use 0 for full output (still byte-capped)."
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Timeout in seconds (default 120, capped at 600)."
                },
                "input_file": {
                    "type": "string",
                    "description": "Path (relative to the root, not `workdir`) of \
                        a file whose content is piped to the child's stdin. \
                        Omitted → stdin is closed, not inherited."
                },
                "output_file": {
                    "type": "string",
                    "description": "Path (relative to the root, not `workdir`) to \
                        write the full, untruncated raw stdout to (missing \
                        parent dirs are created); a `<output_file>.stderr` \
                        sibling is always written alongside. Omitted → nothing \
                        is written to disk; if the response is truncated, the \
                        full text is retained and pollable via the handle named \
                        in the result."
                },
                "workdir": {
                    "type": "string",
                    "description": "Working directory for this call, relative to \
                        the root (must stay under it). Defaults to the root — \
                        use this instead of `bash` just to `cd`."
                },
                "background": {
                    "type": "boolean",
                    "description": "Start the command detached and return a job id \
                        to poll with `poll` instead of blocking. Refused together \
                        with input_file/output_file. Default false."
                }
            },
            "required": ["command"]
        })
    }
    async fn run(&self, input: &str) -> Result<String> {
        self.run_impl(None, "", input).await
    }

    async fn run_for_session(
        &self,
        session: &SessionId,
        request_id: &str,
        input: &str,
    ) -> Result<Vec<ContentPart>> {
        Ok(crate::tools::text_parts(
            self.run_impl(Some(session), request_id, input).await?,
        ))
    }
}

impl CallTool {
    /// `request_id` (#449) is forwarded to the escape-root grant check so a
    /// `Once` approval for `workdir` is only consumed by the call it was
    /// approved for. `session` (#479, ADR-0104 amendment) resolves the
    /// per-profile confinement policy; `None` (the plain [`Tool::run`] path)
    /// resolves against the resolver's process-global default.
    async fn run_impl(
        &self,
        session: Option<&SessionId>,
        request_id: &str,
        input: &str,
    ) -> Result<String> {
        let parsed: CallInput = serde_json::from_str(input)
            .context("invalid input to call: expected {\"command\": string, ...}")?;
        // Fail fast (with a fix) when `command` is a whole shell line rather than
        // a bare executable — otherwise `spawn()` fails with an opaque ENOENT and
        // the model loops the same malformed call (the PR-#446-review failure).
        let bash_available = self.bash_status.as_ref().is_some_and(|s| s.is_enabled());
        validate::check_no_shell(&parsed.command, &parsed.args, bash_available)?;
        let secs = parsed.timeout.unwrap_or(120);
        let dur = std::time::Duration::from_secs(secs.min(MAX_CALL_TIMEOUT_SECONDS));

        if parsed.background {
            if parsed.input_file.is_some() || parsed.output_file.is_some() {
                anyhow::bail!(
                    "call: background=true cannot be combined with input_file/output_file \
                     — a background job streams through `poll`, not a file artifact"
                );
            }
            let cwd = super::resolve_workdir_or_grant(
                &self.root,
                self.extra_roots.as_deref(),
                "call",
                request_id,
                parsed.workdir.as_deref(),
            )?;
            return background::spawn_background(
                &self.root,
                self.sandbox_resolver.as_ref(),
                &self.secret_env,
                &self.jobs,
                session,
                &cwd,
                &parsed.command,
                &parsed.args,
                dur,
            )
            .await;
        }

        let cwd = super::resolve_workdir_or_grant(
            &self.root,
            self.extra_roots.as_deref(),
            "call",
            request_id,
            parsed.workdir.as_deref(),
        )?;
        foreground::run_foreground(
            &self.root,
            self.sandbox_resolver.as_ref(),
            &self.secret_env,
            &self.retained,
            session,
            &cwd,
            &parsed.command,
            &parsed.args,
            &parsed.input_file,
            &parsed.output_file,
            parsed.tail,
            secs,
            dur,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::sandbox;
    use crate::host::MAX_OUTPUT_BYTES;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Isolated per-test root so artifact-writing tests don't collide (and so
    /// their `.entanglement/` litter doesn't accumulate in a shared temp dir).
    struct TempDir {
        path: PathBuf,
    }
    impl TempDir {
        fn new() -> TempDir {
            let id = TEST_DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
            let path =
                std::env::temp_dir().join(format!("entanglement-call-{}-{id}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[tokio::test]
    async fn args_are_passed_verbatim_no_shell_interpretation() {
        // `$HOME`, `;`, `&&`, `|` and a glob must reach the binary as literal
        // argv — a shell would expand/split them.
        let root = std::env::temp_dir();
        let tool = CallTool::new(root);
        let payload = "$HOME; rm -rf / && echo x | cat *.rs";
        let input = serde_json::json!({
            "command": "printf",
            "args": ["%s", payload],
        })
        .to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("[exit 0]"), "got: {out}");
        assert!(out.contains(payload), "argv must be verbatim, got: {out}");
    }

    #[tokio::test]
    async fn missing_binary_is_clean_error_not_panic() {
        let tool = CallTool::new(std::env::temp_dir());
        let input =
            serde_json::json!({ "command": "definitely-not-a-real-binary-xyz" }).to_string();
        let err = tool.run(&input).await.unwrap_err();
        assert!(
            err.to_string().contains("spawning"),
            "expected a clean spawn error, got: {err}"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_is_rendered() {
        let tool = CallTool::new(std::env::temp_dir());
        // `false` exits 1 with no output.
        let input = serde_json::json!({ "command": "false" }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("[exit 1]"), "got: {out}");
    }

    #[tokio::test]
    async fn timeout_kills_long_process() {
        let tool = CallTool::new(std::env::temp_dir());
        let input =
            serde_json::json!({ "command": "sleep", "args": ["30"], "timeout": 1 }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("timed out after 1s"), "got: {out}");
    }

    #[tokio::test]
    async fn timeout_returns_buffered_partial_output() {
        // #169: output emitted before the deadline must accompany the notice.
        // `call` runs no shell, so exec `sh` directly to print then sleep.
        let tool = CallTool::new(std::env::temp_dir());
        let input = serde_json::json!({
            "command": "sh",
            "args": ["-c", "echo early; echo late 1>&2; sleep 30"],
            "timeout": 1,
        })
        .to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("timed out after 1s"), "got: {out}");
        assert!(out.contains("early"), "buffered stdout lost: {out}");
        assert!(
            out.contains("[stderr]") && out.contains("late"),
            "buffered stderr lost: {out}"
        );
    }

    #[tokio::test]
    async fn secret_env_is_scrubbed_from_child() {
        // The no-shell design doesn't protect the env: a plain `env` inherits it.
        // A scrubbed var must be gone while an unrelated var survives (#164).
        std::env::set_var("ENTANGLEMENT_TEST_SECRET_CALL", "leak-me");
        std::env::set_var("ENTANGLEMENT_TEST_PUBLIC_CALL", "public");
        let tool = CallTool::new(std::env::temp_dir())
            .with_secret_env(vec!["ENTANGLEMENT_TEST_SECRET_CALL".to_string()]);
        let input = serde_json::json!({ "command": "env", "tail": 0 }).to_string();
        let out = tool.run(&input).await.unwrap();
        std::env::remove_var("ENTANGLEMENT_TEST_SECRET_CALL");
        std::env::remove_var("ENTANGLEMENT_TEST_PUBLIC_CALL");
        assert!(
            !out.contains("ENTANGLEMENT_TEST_SECRET_CALL"),
            "secret must be scrubbed: {out}"
        );
        assert!(
            out.contains("ENTANGLEMENT_TEST_PUBLIC_CALL=public"),
            "unrelated env kept: {out}"
        );
    }

    #[tokio::test]
    async fn tail_zero_still_byte_capped() {
        // A single stream far larger than the 32 KiB cap must still be bounded.
        let tool = CallTool::new(std::env::temp_dir());
        let big = "x".repeat(MAX_OUTPUT_BYTES * 2);
        let input =
            serde_json::json!({ "command": "printf", "args": ["%s", big], "tail": 0 }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(
            out.len() < MAX_OUTPUT_BYTES + 200,
            "byte cap must fire: {}",
            out.len()
        );
        assert!(out.contains("truncated"), "byte-cap notice expected: {out}");
    }

    #[tokio::test]
    async fn workdir_runs_in_subdirectory() {
        let dir = TempDir::new();
        std::fs::create_dir(dir.path.join("sub")).unwrap();
        std::fs::write(dir.path.join("sub/inner.txt"), "x").unwrap();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({ "command": "ls", "workdir": "sub" }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("inner.txt"), "got: {out}");
    }

    #[tokio::test]
    async fn workdir_escaping_root_is_rejected() {
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({ "command": "ls", "workdir": ".." }).to_string();
        let err = tool.run(&input).await.unwrap_err();
        assert!(
            format!("{err}").contains("escapes working directory"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn workdir_nonexistent_is_rejected() {
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({ "command": "ls", "workdir": "nope" }).to_string();
        let err = tool.run(&input).await.unwrap_err();
        assert!(format!("{err}").contains("not a directory"), "{err}");
    }

    #[tokio::test]
    async fn no_workdir_defaults_to_root() {
        let dir = TempDir::new();
        std::fs::write(dir.path.join("marker.txt"), "x").unwrap();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({ "command": "ls" }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("marker.txt"), "got: {out}");
    }

    #[tokio::test]
    async fn input_file_feeds_child_stdin() {
        let dir = TempDir::new();
        std::fs::write(dir.path.join("in.txt"), "hello-from-file\n").unwrap();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({ "command": "cat", "input_file": "in.txt" }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("hello-from-file"), "got: {out}");
    }

    #[tokio::test]
    async fn no_input_file_closes_stdin_not_inherited() {
        // Regression for the unintentional inherit: without `input_file`, `cat`
        // must see immediate EOF (closed stdin), not block on the engine's real
        // stdin. If it inherited, this would time out instead of exiting clean.
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({ "command": "cat", "timeout": 3 }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(!out.contains("timed out"), "stdin must be closed: {out}");
        assert!(out.contains("[exit 0]"), "got: {out}");
    }

    #[tokio::test]
    async fn missing_input_file_is_clean_error_child_never_spawned() {
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({
            "command": "touch",
            "args": ["spawned-marker"],
            "input_file": "does-not-exist.txt",
        })
        .to_string();
        let err = tool.run(&input).await.unwrap_err();
        assert!(err.to_string().contains("input_file"), "got: {err}");
        assert!(
            !dir.path.join("spawned-marker").exists(),
            "child must not spawn on a bad input_file"
        );
    }

    #[tokio::test]
    async fn escaping_root_paths_error_before_spawn() {
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());

        let in_err = tool
            .run(
                &serde_json::json!({
                    "command": "touch", "args": ["m1"], "input_file": "../escape-in.txt",
                })
                .to_string(),
            )
            .await
            .unwrap_err();
        assert!(in_err.to_string().contains("escapes"), "got: {in_err}");
        assert!(!dir.path.join("m1").exists());

        let out_err = tool
            .run(
                &serde_json::json!({
                    "command": "touch", "args": ["m2"], "output_file": "../escape-out.txt",
                })
                .to_string(),
            )
            .await
            .unwrap_err();
        assert!(out_err.to_string().contains("escapes"), "got: {out_err}");
        assert!(!dir.path.join("m2").exists());
    }

    #[tokio::test]
    async fn output_file_and_stderr_sibling_hold_full_raw_content_under_tail() {
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());
        let full: String = (1..=50).map(|i| format!("line{i}\n")).collect();
        let input = serde_json::json!({
            "command": "printf",
            "args": ["%s", full],
            "tail": 5,
            "output_file": "out/result.txt",
        })
        .to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("earlier lines omitted"), "got: {out}");
        assert!(!out.contains("line1\n"), "response must be tailed: {out}");

        let on_disk = std::fs::read_to_string(dir.path.join("out/result.txt")).unwrap();
        assert_eq!(
            on_disk, full,
            "artifact must hold the full untruncated output"
        );
        assert!(dir.path.join("out/result.txt.stderr").exists());
    }

    #[tokio::test]
    async fn output_file_missing_parent_dirs_are_created() {
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({
            "command": "printf",
            "args": ["%s", "hi\n"],
            "output_file": "nested/deep/out.txt",
        })
        .to_string();
        tool.run(&input).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path.join("nested/deep/out.txt")).unwrap(),
            "hi\n"
        );
        assert!(dir.path.join("nested/deep/out.txt.stderr").exists());
    }

    /// #608: with no `output_file`, a tailed result mints a pollable
    /// retained-output handle instead of a scratch-file path — nothing is
    /// written to disk, and the handle's full text is reachable via `poll`.
    #[tokio::test]
    async fn tailed_output_with_no_file_mints_a_pollable_handle() {
        let dir = TempDir::new();
        let retained = crate::retained_output::RetainedOutputRegistry::new();
        let tool = CallTool::new(dir.path.clone()).with_retained_output(retained.clone());
        let session = SessionId::new("s1");
        // tail=1 on two lines forces truncation, which is what mints a handle.
        let input = serde_json::json!({
            "command": "printf",
            "args": ["%s", "early\nauto-artifact\n"],
            "tail": 1,
        })
        .to_string();
        let out = tool.run_for_session(&session, "r1", &input).await.unwrap();
        let out = entanglement_core::content_text(&out);
        let handle = out
            .split("handle=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .expect("handle in the response");
        assert!(handle.starts_with("o-"), "{handle}");
        assert!(
            !dir.path.join(".entanglement").exists(),
            "nothing should be written to disk"
        );
        let page = retained
            .page(handle, &session, 0, 30)
            .expect("handle is pollable");
        assert!(page.text.contains("early") && page.text.contains("auto-artifact"));
    }

    #[tokio::test]
    async fn small_default_output_names_no_artifact() {
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({ "command": "printf", "args": ["%s", "hi\n"] }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert_eq!(
            out, "[exit 0]\nhi\n",
            "no artifact header on a full, small result"
        );
    }

    /// #608: concurrent truncated calls each mint their own handle — no
    /// collisions, since ids come from the shared `IdGen`, not a filename.
    #[tokio::test]
    async fn concurrent_calls_do_not_collide_on_retained_handles() {
        let dir = TempDir::new();
        let retained = crate::retained_output::RetainedOutputRegistry::new();
        let tool =
            std::sync::Arc::new(CallTool::new(dir.path.clone()).with_retained_output(retained));
        let mut handles = Vec::new();
        for i in 0..8 {
            let tool = tool.clone();
            handles.push(tokio::spawn(async move {
                let input = serde_json::json!({
                    "command": "printf",
                    "args": ["%s", format!("early\ncall-{i}\n")],
                    "tail": 1,
                })
                .to_string();
                tool.run(&input).await.unwrap()
            }));
        }
        let mut seen = std::collections::HashSet::new();
        for h in handles {
            let out = h.await.unwrap();
            let handle = out
                .split("handle=\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
                .unwrap()
                .to_string();
            assert!(seen.insert(handle), "retained-output handles collided");
        }
    }

    #[tokio::test]
    async fn timeout_writes_partial_output_to_artifacts() {
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({
            "command": "sh",
            "args": ["-c", "echo early; echo late 1>&2; sleep 30"],
            "timeout": 1,
            "output_file": "partial.txt",
        })
        .to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("timed out after 1s"), "got: {out}");

        let stdout_on_disk = std::fs::read_to_string(dir.path.join("partial.txt")).unwrap();
        assert!(
            stdout_on_disk.contains("early"),
            "artifact must hold buffered stdout: {stdout_on_disk}"
        );
        let stderr_on_disk = std::fs::read_to_string(dir.path.join("partial.txt.stderr")).unwrap();
        assert!(
            stderr_on_disk.contains("late"),
            "artifact must hold buffered stderr: {stderr_on_disk}"
        );
    }

    /// #606: `background=true` returns a job id instead of blocking, and the
    /// spawned process is reachable via the shared [`JobRegistry`] — the same
    /// mechanism `bash` uses.
    #[tokio::test]
    async fn background_spawns_detached_and_is_pollable() {
        let dir = TempDir::new();
        let jobs = JobRegistry::new();
        let tool = CallTool::new(dir.path.clone()).with_jobs(jobs.clone());
        let input = serde_json::json!({
            "command": "echo",
            "args": ["hi"],
            "background": true,
        })
        .to_string();
        let out = tool.run(&input).await.unwrap();
        let id = out
            .lines()
            .find_map(|l| {
                l.strip_prefix("[background job ")
                    .and_then(|rest| rest.strip_suffix(" started]"))
            })
            .expect("job id in response")
            .to_string();

        let caller = SessionId::new("test-caller");
        for _ in 0..50 {
            let p = jobs
                .poll(&id, &caller, false, 1)
                .await
                .expect("job registered");
            if p.status == crate::host::jobs::JobStatus::Exited(Some(0)) {
                return;
            }
        }
        panic!("background call never exited");
    }

    /// #606, mirroring bash's #617: `timeout` still bounds a backgrounded call.
    #[tokio::test]
    async fn background_is_killed_by_timeout() {
        let dir = TempDir::new();
        let jobs = JobRegistry::new();
        let tool = CallTool::new(dir.path.clone()).with_jobs(jobs.clone());
        let input = serde_json::json!({
            "command": "sleep",
            "args": ["30"],
            "background": true,
            "timeout": 1,
        })
        .to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(
            out.contains("Killed automatically after 1s"),
            "start notice should mention the bound: {out}"
        );
        let id = out
            .lines()
            .find_map(|l| {
                l.strip_prefix("[background job ")
                    .and_then(|rest| rest.strip_suffix(" started]"))
            })
            .expect("job id in response")
            .to_string();

        let caller = SessionId::new("test-caller");
        let p = jobs
            .poll(&id, &caller, false, 5)
            .await
            .expect("job registered");
        assert!(p.timed_out, "job should have been killed by its timeout");
    }

    /// #606: a backgrounded job's output streams through `poll`, not a file
    /// artifact — `input_file` (whose stdin-feed has no running wait to pipe
    /// into once the call has already returned) is refused up front, before
    /// anything spawns.
    #[tokio::test]
    async fn background_rejects_input_file() {
        let dir = TempDir::new();
        std::fs::write(dir.path.join("in.txt"), "hi").unwrap();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({
            "command": "cat",
            "background": true,
            "input_file": "in.txt",
        })
        .to_string();
        let err = tool.run(&input).await.unwrap_err();
        assert!(err.to_string().contains("background=true"), "{err}");
    }

    /// #606: same refusal for `output_file` — a backgrounded job has no
    /// foreground wait to persist an artifact from.
    #[tokio::test]
    async fn background_rejects_output_file() {
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone());
        let input = serde_json::json!({
            "command": "echo",
            "args": ["hi"],
            "background": true,
            "output_file": "out.txt",
        })
        .to_string();
        let err = tool.run(&input).await.unwrap_err();
        assert!(err.to_string().contains("background=true"), "{err}");
        assert!(!dir.path.join("out.txt").exists());
    }

    fn bwrap_policy(network: bool) -> SandboxPolicy {
        SandboxPolicy {
            backend: sandbox::SandboxBackend::Bubblewrap,
            network,
        }
    }

    /// ADR-0104: a sandboxed `call` can still write inside the bind-mounted
    /// project root, but the rest of the filesystem is read-only. `outside` is
    /// deliberately under `/var/tmp`, not `/tmp` — the recipe gives the latter
    /// a fresh empty tmpfs, which would fail for the wrong reason (path
    /// doesn't exist) rather than the read-only-bind reason under test.
    #[tokio::test]
    async fn sandbox_confines_writes_to_root() {
        if !sandbox::bwrap_available() {
            eprintln!("skipping: bwrap not installed");
            return;
        }
        let dir = TempDir::new();
        let outside = tempfile::Builder::new()
            .prefix("entanglement-sandbox-call-test-")
            .tempdir_in("/var/tmp")
            .unwrap();
        let tool = CallTool::new(dir.path.clone()).with_sandbox(bwrap_policy(false));

        let input = serde_json::json!({ "command": "touch", "args": ["inside.txt"] }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("[exit 0]"), "{out}");
        assert!(dir.path.join("inside.txt").exists());

        let leak_path = outside.path().join("leak.txt");
        let input = serde_json::json!({
            "command": "touch",
            "args": [leak_path.to_string_lossy()],
        })
        .to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(
            !out.contains("[exit 0]"),
            "write outside root should fail: {out}"
        );
        assert!(
            !leak_path.exists(),
            "sandbox must not allow writes outside the project root"
        );
    }

    /// ADR-0104: sandboxed network is cut by default. `call` has no shell, so
    /// exec `sh` directly to reuse bash's `/dev/tcp` trick — needs no external
    /// network binary (`curl`/`nc`).
    #[tokio::test]
    async fn sandbox_cuts_network_by_default() {
        if !sandbox::bwrap_available() {
            eprintln!("skipping: bwrap not installed");
            return;
        }
        let dir = TempDir::new();
        let tool = CallTool::new(dir.path.clone()).with_sandbox(bwrap_policy(false));
        let input = serde_json::json!({
            "command": "sh",
            "args": ["-c", "exec 3<>/dev/tcp/1.1.1.1/80"],
            "timeout": 5,
        })
        .to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(
            !out.contains("[exit 0]"),
            "network must be unreachable when sandboxed without network:true: {out}"
        );
    }

    /// ADR-0104 §6: the process-group timeout/kill path must still tear down a
    /// sandboxed command's whole tree, not just the outer `bwrap` process.
    #[tokio::test]
    async fn sandbox_timeout_still_kills_the_whole_tree() {
        if !sandbox::bwrap_available() {
            eprintln!("skipping: bwrap not installed");
            return;
        }
        let tool = CallTool::new(std::env::temp_dir()).with_sandbox(bwrap_policy(false));
        let input =
            serde_json::json!({ "command": "sleep", "args": ["30"], "timeout": 1 }).to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("timed out after 1s"), "got: {out}");
    }

    /// ADR-0104: `call`'s no-shell argv-exec guarantee (a shell metacharacter
    /// reaches the binary literally, never interpreted) must hold identically
    /// when sandboxed — bwrap wraps the exec, it must not reintroduce a shell.
    #[tokio::test]
    async fn sandbox_preserves_no_shell_argv_semantics() {
        if !sandbox::bwrap_available() {
            eprintln!("skipping: bwrap not installed");
            return;
        }
        let tool = CallTool::new(std::env::temp_dir()).with_sandbox(bwrap_policy(false));
        let payload = "$HOME; rm -rf / && echo x | cat *.rs";
        let input = serde_json::json!({
            "command": "printf",
            "args": ["%s", payload],
        })
        .to_string();
        let out = tool.run(&input).await.unwrap();
        assert!(out.contains("[exit 0]"), "got: {out}");
        assert!(out.contains(payload), "argv must be verbatim, got: {out}");
    }
}
