//! Session creation: an explicit `Spawn` (child or compaction-successor root)
//! and the `SpawnBlank` unknown-session policy's fresh-root auto-create.

use entanglement_provider::UserId;
use tokio::sync::mpsc;

use super::Supervisor;
use crate::holly::routing::emit_supervisor_error;
use crate::holly::{DEFAULT_AGENT, DEFAULT_MODE, SESSION_CMD_CAPACITY};
use crate::protocol::{SessionId, SessionInfo};
use crate::session::{session_loop, SessionCmd};

impl Supervisor {
    /// `Spawn`: start `child` under `agent`, queueing `prompt` as its first turn.
    pub(super) async fn spawn(
        &mut self,
        child: &SessionId,
        parent: &Option<SessionId>,
        predecessor: &Option<SessionId>,
        agent: &str,
        prompt: &str,
        user: &Option<UserId>,
    ) {
        // A duplicate spawn for a live child is a no-op (the child already runs).
        if self.sessions.contains_key(child) {
            return;
        }
        // A retired id is single-use; never respawn it (ADR-0028, issue #105).
        if self.closed.contains(child) {
            emit_supervisor_error(
                &self.events,
                &self.seqs,
                child,
                "cannot spawn a closed session id (ids are single-use)",
            );
            return;
        }
        // An unknown spawn target must not silently escalate to `build` (the
        // most-privileged default): `resolve` would fall back there, so a
        // typo'd `Spawn` would launch a full coding agent. `get` + a
        // supervisor error refuses instead (#119). The lazy-Prompt path
        // below still uses `resolve`, but only for a genuinely fresh id —
        // a known sub-agent child refuses the same way (issue #639).
        let resolved_agent = match self.cfg.agents.get(agent) {
            Some(p) => p.clone(),
            None => {
                emit_supervisor_error(
                    &self.events,
                    &self.seqs,
                    child,
                    &format!("cannot spawn unknown agent `{agent}`"),
                );
                return;
            }
        };
        // `parent = None` is a root spawn (the `/compact` successor fork,
        // ADR-0110): it records `predecessor` for lineage but joins no spawn
        // sub-tree, so a `CloseSession` on the source never cascades onto it.
        let parent = parent.clone();
        let is_root = parent.is_none();
        // A user is a spawn-time fact inherited from the lineage, never
        // re-specified by the spawning caller (#522): a child inherits its
        // live parent's user, a fresh root successor (predecessor set, no
        // parent — the `/compact` fork, ADR-0110) inherits the source's, and
        // only a genuine fresh root (neither set) takes the message's own
        // `user` field — the multi-user embedder's session-creation entry
        // point.
        let inherited_user = parent
            .as_ref()
            .and_then(|p| self.session_meta.get(p))
            .and_then(|m| m.user)
            .or_else(|| {
                predecessor
                    .as_ref()
                    .and_then(|p| self.session_meta.get(p))
                    .and_then(|m| m.user)
            });
        let effective_user = inherited_user.or_else(|| user.clone());
        // ADR-0207 §6: mode applies to the whole spawn sub-tree — a
        // spawned child starts under its parent's live mode. A
        // compaction successor (predecessor set, no parent — the
        // `/compact` fork, ADR-0110) is the same session continuing, so
        // it inherits the source's mode the same way `effective_user`
        // falls back to it above. Neither known ⇒ a genuine fresh root,
        // `DEFAULT_MODE`.
        let initial_mode = parent
            .as_ref()
            .and_then(|p| self.modes.get(p))
            .or_else(|| predecessor.as_ref().and_then(|p| self.modes.get(p)))
            .cloned()
            .unwrap_or_else(|| DEFAULT_MODE.to_string());
        self.modes.insert(child.clone(), initial_mode.clone());
        // Record the parent link *before* spawning so it's in place for any
        // later lazy path, and so the child starts under the requested agent.
        self.parent_links.insert(child.clone(), parent.clone());
        // A compaction successor (ADR-0205): remember which session it took
        // over from, so a late tool result addressed to that now-retired
        // source is redirected here rather than refused.
        if let Some(source) = predecessor.as_ref() {
            self.successors.insert(source.clone(), child.clone());
        }
        self.session_meta.insert(SessionInfo {
            session: child.clone(),
            parent: parent.clone(),
            predecessor: predecessor.clone(),
            agent: resolved_agent.name.clone(),
            root: is_root,
            user: effective_user.clone(),
        });
        let (stx, srx) = mpsc::channel::<SessionCmd>(SESSION_CMD_CAPACITY);
        let ev = self.events.clone();
        let cfg2 = self.cfg.clone();
        let sid = child.clone();
        let predecessor = predecessor.clone();
        let parent_for_loop = parent.clone();
        let seqs2 = self.seqs.clone();
        let activity2 = self.activity.clone();
        let forks = self.fork_tx.clone();
        tokio::spawn(async move {
            session_loop(
                sid,
                srx,
                ev,
                cfg2,
                resolved_agent,
                None,
                parent_for_loop,
                predecessor,
                effective_user,
                initial_mode,
                seqs2,
                activity2,
                forks,
            )
            .await
        });
        // Queue the initial prompt; the child drains it after its lifecycle
        // events. Spawn prompts are text-only (#197). An *empty* prompt
        // queues no turn at all (#674, ADR-0174): a head authoring a
        // trusted `Spawn` just to bind a session's identity (`user`)
        // relays the client's real `Prompt` separately — running a model
        // turn on "" would then stash that prompt as mid-turn steering.
        if !prompt.is_empty() {
            let content = vec![entanglement_provider::ContentPart::text(prompt.to_string())];
            let _ = stx.send(SessionCmd::Prompt(content)).await;
        }
        self.sessions.insert(child.clone(), stx);
        // Mirror the child→parent edge onto the parent's live `children`
        // list. Best-effort: if the parent task already exited, the edge
        // still lives in `parent_links`; the mirror is a convenience view.
        if let Some(parent_id) = parent.as_ref() {
            if let Some(ptx) = self.sessions.get(parent_id) {
                let _ = ptx.send(SessionCmd::ChildSpawned(child.clone())).await;
            }
        }
    }

    /// The [`UnknownSession::SpawnBlank`][crate::UnknownSession::SpawnBlank]
    /// path for a non-closed id with no live task: auto-create a blank root, or
    /// refuse (emitting a supervisor `Error`) and return `false`.
    pub(super) fn spawn_lazy_root(&mut self, session_id: &SessionId) -> bool {
        // A `parent_links` entry with a real parent means this id is a
        // known sub-agent child, not a fresh id — hibernation deliberately
        // leaves this map entry in place (unlike `session_meta`) so this
        // check can catch it (issue #639). Silently blank-respawning it
        // here would both discard its history and re-create it under
        // `build`'s permission mode and tool set, the exact
        // escalation the unknown-`Spawn`-target case above refuses.
        // Refuse and point the caller at `Resume` instead.
        if self
            .parent_links
            .get(session_id)
            .cloned()
            .flatten()
            .is_some()
        {
            emit_supervisor_error(
                &self.events,
                &self.seqs,
                session_id,
                "session id belongs to a hibernated sub-agent child; Resume it before prompting instead of respawning it blank",
            );
            return false;
        }
        // A genuinely fresh id (no `parent_links` entry, or one recorded
        // with no parent — e.g. a hibernated `/compact` successor root)
        // keeps the lazy-Prompt path's single-user convenience: an unknown
        // session id auto-creates a blank root under `build`.
        let agent = self.cfg.agents.resolve(DEFAULT_AGENT);
        self.session_meta.insert(SessionInfo {
            session: session_id.clone(),
            parent: None,
            predecessor: None,
            agent: agent.name.clone(),
            root: true,
            user: None,
        });
        let (stx, srx) = mpsc::channel::<SessionCmd>(SESSION_CMD_CAPACITY);
        let ev = self.events.clone();
        let cfg2 = self.cfg.clone();
        let sid = session_id.clone();
        let seqs2 = self.seqs.clone();
        let activity2 = self.activity.clone();
        let forks = self.fork_tx.clone();
        tokio::spawn(async move {
            session_loop(
                sid,
                srx,
                ev,
                cfg2,
                agent,
                None,
                None,
                None,
                None,
                DEFAULT_MODE.to_string(),
                seqs2,
                activity2,
                forks,
            )
            .await
        });
        self.sessions.insert(session_id.clone(), stx);
        true
    }
}
