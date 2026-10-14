//! Session creation: an explicit `Spawn` (child or compaction-successor root)
//! and the `SpawnBlank` unknown-session policy's fresh-root auto-create.

use entanglement_provider::{ResolvedModel, UserId};
use tokio::sync::mpsc;

use super::starting_mode::{starting_mode, Lineage, Requested};
use super::Supervisor;
use crate::holly::routing::emit_supervisor_error;
use crate::holly::{DEFAULT_AGENT, DEFAULT_MODE, SESSION_CMD_CAPACITY};
use crate::protocol::{ModelChoice, SessionId, SessionInfo};
use crate::session::{session_loop, SessionCmd, SessionStart};

impl Supervisor {
    /// `Spawn`: start `child` under `agent`, queueing `prompt` as its first turn.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn spawn(
        &mut self,
        child: &SessionId,
        parent: &Option<SessionId>,
        predecessor: &Option<SessionId>,
        agent: &str,
        prompt: &str,
        user: &Option<UserId>,
        model: &Option<ModelChoice>,
        mode: &Option<String>,
        workdir: Option<&str>,
    ) {
        // Taken first so no early return below can leave it parked.
        let mut inherited = self.inherited.remove(child);
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
        // Where the new session sits in the spawn tree (ADR-0234): a
        // compaction successor takes its predecessor's place — a root stays a
        // root, a sub-agent stays under its parent — and a child naming a
        // parent that compacted away joins that parent's successor.
        let parent = self.placed_parent(parent.as_ref(), predecessor.as_ref());
        let is_root = parent.is_none();
        // A user is a spawn-time fact inherited from the lineage, never
        // re-specified by the spawning caller (#522): a child inherits its
        // live parent's user (a sub-agent's compaction successor included,
        // ADR-0234), a root successor (predecessor set, no parent — the
        // `/compact` fork, ADR-0110) inherits the source's, and
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
        // Resolved here, before any supervisor state is recorded, so a bad
        // choice refuses the spawn outright (#725) — running the first turn
        // on a model the caller didn't ask for would be a silent cost/quality
        // swap no later `Error` can undo.
        let spawn_model = match model {
            Some(choice) => {
                match self.resolve_spawn_model(child, choice, effective_user.as_ref()) {
                    Some(resolved) => Some(resolved),
                    None => return,
                }
            }
            // A compaction successor (#768) starts on its source's already
            // bound backend: nothing to resolve, so nothing that can refuse
            // the spawn of a session whose source is being retired.
            None => inherited.as_mut().and_then(|i| i.model.take()),
        };
        // Refused before any state is recorded, like a bad `model` (#763):
        // starting under a misspelled mode would grade every call against a
        // table that doesn't exist. Core only knows the names its ladder
        // lists, so an empty ladder carries the name opaquely, as `SetMode`.
        if let Some(m) = mode {
            let ladder = &self.cfg.mode_ladder;
            if !ladder.is_empty() && !ladder.contains(m) {
                emit_supervisor_error(
                    &self.events,
                    &self.seqs,
                    child,
                    &format!("cannot spawn in unknown mode `{m}`"),
                );
                return;
            }
        }
        // Refused before any state is recorded too, like `model`/`mode`.
        let Ok(initial_workdir) =
            self.spawn_workdir(child, parent.as_ref(), workdir, inherited.as_ref())
        else {
            return;
        };
        // ADR-0207 §6: mode applies to the whole spawn sub-tree — a child
        // starts under its parent's live mode, narrowed (never widened) by
        // its requested mode or agent's `default_mode`; a compaction
        // successor (ADR-0110) is the same session continuing, so it keeps
        // the source's mode; a genuine fresh root starts in its requested
        // mode, else its agent's hint clamped to `DEFAULT_MODE` (#763,
        // ADR-0218).
        let mode_of = |id: &SessionId| {
            self.modes
                .get(id)
                .map_or(DEFAULT_MODE, String::as_str)
                .to_string()
        };
        let (parent_mode, source_mode) = (
            parent.as_ref().map(mode_of),
            predecessor.as_ref().map(mode_of),
        );
        // A successor is checked first: since ADR-0234 a sub-agent's
        // successor has a parent too, but it is still the same session going
        // on, so it keeps the source's mode rather than re-deriving a child's.
        let lineage = match (&source_mode, &parent_mode) {
            (Some(s), _) => Lineage::Successor(s),
            (None, Some(p)) => Lineage::Child(p),
            (None, None) => Lineage::Root,
        };
        let requested = Requested {
            explicit: mode.as_deref(),
            hint: resolved_agent.default_mode.as_deref(),
        };
        let initial_mode = starting_mode(lineage, requested, &self.cfg.mode_ladder);
        self.modes.insert(child.clone(), initial_mode.clone());
        // Record the parent link *before* spawning so it's in place for any
        // later lazy path, and so the child starts under the requested agent.
        self.parent_links.insert(child.clone(), parent.clone());
        // A compaction successor (ADR-0205): remember which session it took
        // over from, so a late tool result addressed to that now-retired
        // source is redirected here rather than refused.
        if let Some(source) = predecessor.as_ref() {
            self.session_meta.record_successor(source, child);
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
        let predecessor_for_loop = predecessor.clone();
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
                SessionStart {
                    agent: resolved_agent,
                    initial_session: None,
                    parent: parent_for_loop,
                    predecessor: predecessor_for_loop,
                    user: effective_user,
                    initial_mode,
                    spawn_model,
                    inherited,
                    initial_workdir,
                },
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
        // Before the fork's `CloseSession` for the source is even dequeued,
        // so its cascade finds nothing left to kill (ADR-0234).
        if let Some(source) = predecessor.as_ref() {
            self.adopt_children(source, child).await;
        }
    }

    /// Resolve a `Spawn`'s explicit model choice under the session's
    /// effective user, or emit a supervisor `Error` for `child` and return
    /// `None` — the same refusal shape an unknown spawn `agent` gets.
    fn resolve_spawn_model(
        &self,
        child: &SessionId,
        choice: &ModelChoice,
        user: Option<&UserId>,
    ) -> Option<ResolvedModel> {
        let ModelChoice { provider, model } = choice;
        let result = match self.cfg.model_resolver.as_ref() {
            Some(resolver) => resolver(user, provider, model),
            None => Err("model switching is not supported by this engine".to_string()),
        };
        result
            .map_err(|e| {
                emit_supervisor_error(
                    &self.events,
                    &self.seqs,
                    child,
                    &format!("cannot spawn on model `{provider}/{model}`: {e}"),
                );
            })
            .ok()
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
        let requested = Requested {
            explicit: None,
            hint: agent.default_mode.as_deref(),
        };
        let initial_mode = starting_mode(Lineage::Root, requested, &self.cfg.mode_ladder);
        self.modes.insert(session_id.clone(), initial_mode.clone());
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
                SessionStart {
                    agent,
                    initial_session: None,
                    parent: None,
                    predecessor: None,
                    user: None,
                    initial_mode,
                    spawn_model: None,
                    inherited: None,
                    initial_workdir: None,
                },
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
