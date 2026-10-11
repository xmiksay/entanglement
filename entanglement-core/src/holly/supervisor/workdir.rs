//! Session working directories (#764, ADR-0224): validating a `SetWorkdir`
//! or `Spawn.workdir` through the runtime's resolver, and the supervisor's
//! cache a child's inherited workdir is read from.

use super::Supervisor;
use crate::holly::routing::emit_supervisor_error;
use crate::protocol::{ApprovalScope, InMsg, SessionId};
use crate::session::Inherited;

impl Supervisor {
    /// Validate a `SetWorkdir` on arrival — against the session's latest
    /// workdir, so `/cd ..` means what the user just saw — and hand it on
    /// with `dir` made absolute. Every other message passes through. A bad
    /// directory emits a supervisor `Error` and the message goes no further.
    pub(super) fn validate_set_workdir(&mut self, msg: InMsg) -> Option<InMsg> {
        let InMsg::SetWorkdir {
            session,
            dir,
            approve,
        } = msg
        else {
            return Some(msg);
        };
        let current = self.workdirs.get(&session).map(String::as_str);
        match self.resolve_workdir(current, &dir, approve) {
            Ok(abs) => {
                // Cached now, though the session applies it only once idle:
                // a child spawned in between already inherits it.
                self.workdirs.insert(session.clone(), abs.clone());
                Some(InMsg::SetWorkdir {
                    session,
                    dir: abs,
                    approve,
                })
            }
            Err(e) => {
                emit_supervisor_error(
                    &self.events,
                    &self.seqs,
                    &session,
                    &format!("cannot change workdir to `{dir}`: {e}"),
                );
                None
            }
        }
    }

    /// The workdir a spawned session starts in: its own `Spawn.workdir`
    /// (validated relative to its parent's, with no approval), else what a
    /// successor/fork inherited, else its parent's. `Err` after emitting the
    /// refusal, so the caller aborts the spawn before recording anything.
    pub(super) fn spawn_workdir(
        &mut self,
        child: &SessionId,
        parent: Option<&SessionId>,
        explicit: Option<&str>,
        inherited: Option<&Inherited>,
    ) -> Result<Option<String>, ()> {
        let parent_dir = parent.and_then(|p| self.workdirs.get(p)).cloned();
        let workdir = match explicit {
            Some(dir) => match self.resolve_workdir(parent_dir.as_deref(), dir, None) {
                Ok(abs) => Some(abs),
                Err(e) => {
                    emit_supervisor_error(
                        &self.events,
                        &self.seqs,
                        child,
                        &format!("cannot spawn in workdir `{dir}`: {e}"),
                    );
                    return Err(());
                }
            },
            None => inherited.and_then(|i| i.workdir.clone()).or(parent_dir),
        };
        if let Some(dir) = &workdir {
            self.workdirs.insert(child.clone(), dir.clone());
        }
        Ok(workdir)
    }

    fn resolve_workdir(
        &self,
        current: Option<&str>,
        dir: &str,
        approve: Option<ApprovalScope>,
    ) -> Result<String, String> {
        match &self.cfg.workdir_resolver {
            Some(resolve) => resolve(current, dir, approve),
            None => Err("this engine has no workdir resolver".to_string()),
        }
    }
}
