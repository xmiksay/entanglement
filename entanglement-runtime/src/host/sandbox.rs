//! Optional OS-level process confinement for the `bash`/`call` exec pair via
//! bubblewrap (`bwrap`) — [ADR-0104][adr]. `SandboxPolicy::none()` (the
//! default, [ADR-0047][adr47]) leaves command construction untouched, so
//! unsandboxed execution stays byte-for-byte what it was before this module
//! existed. `SandboxPolicy` with [`SandboxBackend::Bubblewrap`] rewrites the
//! spawn into `bwrap <fixed recipe> -- <real argv>` — **fail-closed**: there is
//! no fallback to unsandboxed execution when `bwrap` can't be entered (missing
//! binary, unprivileged user namespaces disabled), so a sandboxed spawn that
//! can't run simply errors like any other missing binary (ADR-0016).
//!
//! [adr]: ../../../../docs/adr/0104-bubblewrap-sandbox-for-bash-call.md
//! [adr47]: ../../../../docs/adr/0047-local-trust-boundary.md

use std::path::Path;

use tokio::process::Command;

/// Which confinement mechanism (if any) wraps a spawned `bash`/`call` child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxBackend {
    /// No confinement — today's behavior, the [ADR-0047][adr47] default.
    ///
    /// [adr47]: ../../../../docs/adr/0047-local-trust-boundary.md
    #[default]
    None,
    /// Confine via bubblewrap (`bwrap` on `PATH`).
    Bubblewrap,
}

/// A single confinement policy for `bash`/`call` — the process-global
/// `ENTANGLEMENT_SANDBOX` default (ADR-0104), a per-profile `sandbox:`
/// override resolved against it ([`SandboxPolicy::resolve_profile_override`],
/// #479, ADR-0104 amendment), or the frozen ancestor floor a spawned child
/// clamps to ([`SandboxPolicy::most_confined`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SandboxPolicy {
    pub backend: SandboxBackend,
    /// Share the host network namespace with the sandboxed command. Only
    /// meaningful when `backend != None`; ignored otherwise. Default `false`
    /// (network cut) — the ADR's default-closed egress policy.
    pub network: bool,
}

impl SandboxPolicy {
    /// No confinement — the default.
    pub fn none() -> Self {
        Self::default()
    }

    /// Read the policy from `ENTANGLEMENT_SANDBOX` (`bwrap`/`bubblewrap` ⇒
    /// confined, anything else/unset ⇒ none) and `ENTANGLEMENT_SANDBOX_NETWORK`
    /// (`1` ⇒ share the host network namespace when confined).
    pub fn from_env() -> Self {
        let backend = match std::env::var("ENTANGLEMENT_SANDBOX").as_deref() {
            Ok("bwrap") | Ok("bubblewrap") => SandboxBackend::Bubblewrap,
            _ => SandboxBackend::None,
        };
        let network = std::env::var("ENTANGLEMENT_SANDBOX_NETWORK").as_deref() == Ok("1");
        Self { backend, network }
    }

    pub fn is_sandboxed(&self) -> bool {
        self.backend != SandboxBackend::None
    }

    /// Resolve this policy (typically the process-global `ENTANGLEMENT_SANDBOX`
    /// default) against a profile's optional `sandbox:` frontmatter override
    /// (#479, ADR-0104 amendment): `None` ⇒ inherit `self` unchanged;
    /// `Some("bwrap" | "bubblewrap")` ⇒ confined, keeping `self.network`;
    /// `Some("none")` ⇒ unconfined. Any other string is a load-time error
    /// `agents::build_profile` already rejects, so a profile's `sandbox` field
    /// never carries one here — matched as a no-op fallback rather than a
    /// panic, since this is not the validating boundary.
    pub fn resolve_profile_override(&self, over: Option<&str>) -> SandboxPolicy {
        match over {
            None => *self,
            Some("bwrap") | Some("bubblewrap") => SandboxPolicy {
                backend: SandboxBackend::Bubblewrap,
                network: self.network,
            },
            Some("none") => SandboxPolicy::none(),
            Some(_) => *self,
        }
    }

    /// Confinement rank for the spawn-chain clamp (#479, ADR-0104 amendment):
    /// higher = more confined. Network-sharing bubblewrap sits strictly
    /// between unconfined and network-cut bubblewrap — it still isolates the
    /// filesystem/pid/ipc/uts/cgroup namespaces, just not network.
    fn confinement_rank(&self) -> u8 {
        match (self.backend, self.network) {
            (SandboxBackend::None, _) => 0,
            (SandboxBackend::Bubblewrap, true) => 1,
            (SandboxBackend::Bubblewrap, false) => 2,
        }
    }

    /// The more-confined of `self`/`other` (#479, ADR-0104 amendment): a
    /// spawned child's effective sandbox is never less confined than its
    /// parent's, mirroring ADR-0024's permission privilege ceiling for
    /// confinement instead of permission grade.
    pub fn most_confined(self, other: SandboxPolicy) -> SandboxPolicy {
        if self.confinement_rank() >= other.confinement_rank() {
            self
        } else {
            other
        }
    }
}

/// Build the `Command` that will run `program`+`args` under `policy`, rooted
/// at `root` (the project root a sandboxed call may read/write — bind-mounted
/// read-write at the same absolute path, so [`super::resolve_under_root`]'s
/// symlink-safe containment keeps working unmodified inside the sandbox,
/// ADR-0104 §5). `SandboxBackend::None` returns a plain, unwrapped
/// `Command::new(program)` — identical to pre-sandbox behavior.
pub fn command(policy: &SandboxPolicy, root: &Path, program: &str, args: &[String]) -> Command {
    match policy.backend {
        SandboxBackend::None => {
            let mut cmd = Command::new(program);
            cmd.args(args);
            cmd
        }
        SandboxBackend::Bubblewrap => {
            let mut cmd = Command::new("bwrap");
            // Mount order matters to bwrap: a later mount shadows an earlier
            // one at an overlapping path. `root` commonly lives under `/tmp`
            // (test temp dirs, some project layouts), so the root bind must
            // come *after* the fresh `/tmp` tmpfs or the tmpfs would shadow it
            // back to empty/read-only.
            cmd.arg("--ro-bind").arg("/").arg("/");
            cmd.arg("--dev").arg("/dev");
            cmd.arg("--proc").arg("/proc");
            cmd.arg("--tmpfs").arg("/tmp");
            cmd.arg("--bind").arg(root).arg(root);
            cmd.arg("--unshare-pid");
            cmd.arg("--unshare-ipc");
            cmd.arg("--unshare-uts");
            cmd.arg("--unshare-cgroup");
            if !policy.network {
                cmd.arg("--unshare-net");
            }
            cmd.arg("--die-with-parent");
            cmd.arg("--new-session");
            cmd.arg("--").arg(program).args(args);
            cmd
        }
    }
}

/// Whether `bwrap` is on `PATH` and runs. Tests elsewhere in `host` that need
/// a real sandboxed spawn call this and skip themselves (rather than fail the
/// suite) when it's absent — bubblewrap isn't guaranteed present in every
/// dev/CI environment.
#[cfg(test)]
pub(crate) fn bwrap_available() -> bool {
    std::process::Command::new("bwrap")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(cmd: &Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn none_backend_passes_through_unwrapped() {
        let policy = SandboxPolicy::none();
        let cmd = command(
            &policy,
            Path::new("/proj"),
            "sh",
            &["-c".into(), "echo hi".into()],
        );
        assert_eq!(cmd.as_std().get_program(), "sh");
        assert_eq!(argv(&cmd), vec!["-c", "echo hi"]);
    }

    #[test]
    fn bubblewrap_wraps_program_after_double_dash() {
        let policy = SandboxPolicy {
            backend: SandboxBackend::Bubblewrap,
            network: false,
        };
        let cmd = command(
            &policy,
            Path::new("/proj"),
            "sh",
            &["-c".into(), "echo hi".into()],
        );
        assert_eq!(cmd.as_std().get_program(), "bwrap");
        let args = argv(&cmd);
        let dash_dash = args.iter().position(|a| a == "--").expect("has --");
        assert_eq!(&args[dash_dash + 1..], ["sh", "-c", "echo hi"]);
    }

    #[test]
    fn bubblewrap_binds_root_read_write_and_rest_read_only() {
        let policy = SandboxPolicy {
            backend: SandboxBackend::Bubblewrap,
            network: false,
        };
        let cmd = command(&policy, Path::new("/proj/root"), "true", &[]);
        let args = argv(&cmd);
        assert_eq!(&args[0..3], ["--ro-bind", "/", "/"]);
        let bind_idx = args.iter().position(|a| a == "--bind").unwrap();
        assert_eq!(
            &args[bind_idx + 1..bind_idx + 3],
            ["/proj/root", "/proj/root"]
        );
    }

    #[test]
    fn bubblewrap_cuts_network_by_default() {
        let policy = SandboxPolicy {
            backend: SandboxBackend::Bubblewrap,
            network: false,
        };
        let cmd = command(&policy, Path::new("/proj"), "true", &[]);
        assert!(argv(&cmd).iter().any(|a| a == "--unshare-net"));
    }

    #[test]
    fn bubblewrap_network_true_shares_host_netns() {
        let policy = SandboxPolicy {
            backend: SandboxBackend::Bubblewrap,
            network: true,
        };
        let cmd = command(&policy, Path::new("/proj"), "true", &[]);
        assert!(!argv(&cmd).iter().any(|a| a == "--unshare-net"));
    }

    // Both env-var cases live in one test (rather than two `#[test]`s) since
    // `std::env::set_var`/`remove_var` on the same key would otherwise race
    // across parallel test threads in this binary.
    #[test]
    fn from_env_reads_backend_and_network() {
        std::env::remove_var("ENTANGLEMENT_SANDBOX");
        std::env::remove_var("ENTANGLEMENT_SANDBOX_NETWORK");
        let policy = SandboxPolicy::from_env();
        assert_eq!(policy.backend, SandboxBackend::None);
        assert!(!policy.is_sandboxed());

        std::env::set_var("ENTANGLEMENT_SANDBOX", "bwrap");
        std::env::set_var("ENTANGLEMENT_SANDBOX_NETWORK", "1");
        let policy = SandboxPolicy::from_env();
        std::env::remove_var("ENTANGLEMENT_SANDBOX");
        std::env::remove_var("ENTANGLEMENT_SANDBOX_NETWORK");
        assert_eq!(policy.backend, SandboxBackend::Bubblewrap);
        assert!(policy.is_sandboxed());
        assert!(policy.network);
    }

    #[test]
    fn profile_override_none_inherits_the_base_policy() {
        let base = SandboxPolicy {
            backend: SandboxBackend::Bubblewrap,
            network: true,
        };
        assert_eq!(base.resolve_profile_override(None), base);
        let unsandboxed = SandboxPolicy::none();
        assert_eq!(unsandboxed.resolve_profile_override(None), unsandboxed);
    }

    #[test]
    fn profile_override_bwrap_confines_regardless_of_base() {
        let unsandboxed = SandboxPolicy::none();
        let confined = unsandboxed.resolve_profile_override(Some("bwrap"));
        assert_eq!(confined.backend, SandboxBackend::Bubblewrap);
        // Network posture is inherited from the base, not forced either way.
        assert!(!confined.network);
        let base_with_network = SandboxPolicy {
            backend: SandboxBackend::None,
            network: true,
        };
        assert!(
            base_with_network
                .resolve_profile_override(Some("bubblewrap"))
                .network
        );
    }

    #[test]
    fn profile_override_none_string_forces_unconfined() {
        let base = SandboxPolicy {
            backend: SandboxBackend::Bubblewrap,
            network: false,
        };
        assert_eq!(
            base.resolve_profile_override(Some("none")),
            SandboxPolicy::none()
        );
    }

    #[test]
    fn most_confined_picks_the_stricter_policy() {
        let none = SandboxPolicy::none();
        let bwrap_net = SandboxPolicy {
            backend: SandboxBackend::Bubblewrap,
            network: true,
        };
        let bwrap_no_net = SandboxPolicy {
            backend: SandboxBackend::Bubblewrap,
            network: false,
        };
        assert_eq!(none.most_confined(bwrap_net), bwrap_net);
        assert_eq!(bwrap_net.most_confined(none), bwrap_net);
        assert_eq!(bwrap_net.most_confined(bwrap_no_net), bwrap_no_net);
        assert_eq!(bwrap_no_net.most_confined(bwrap_net), bwrap_no_net);
        // Ties keep either side (both branches return an equally-confined policy).
        assert_eq!(none.most_confined(none), none);
    }
}
