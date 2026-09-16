//! Live re-pin of a running session's advertising mode and discovery
//! strategy — the TUI `/set` dialog's API. Explicit and user-confirmed: the
//! next round's tools array and system-prompt note change, so the provider
//! cache is rebuilt once. The encoding stays wire-derived and is never
//! re-pinned.

use entanglement_core::{Discovery, SessionId, ToolAdvertising};

use super::{AdvertisingState, Encoding, Pinned, SessionToolAdvertising};

/// A re-pin requested before the session's first resolution.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Override {
    mode: Option<ToolAdvertising>,
    discovery: Option<Discovery>,
}

/// What shapes the array, per pinned facts. Two pins with the same shape
/// advertise the same bytes, so a re-pin between them needs no rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Full,
    /// `client_side` + `append`: kernel plus the discovered tail.
    Tail,
    /// `client_side` + `native_first`/`invoke`: kernel plus `invoke`.
    Fixed(Discovery),
    /// `anthropic_native`/`responses_native`: deferred full surface.
    Native,
}

impl Shape {
    fn of(p: Pinned) -> Self {
        match (p.mode, p.encoding) {
            (ToolAdvertising::Full, _) => Shape::Full,
            (ToolAdvertising::ToolSearch, Encoding::ClientSide) => match p.discovery {
                Discovery::Append => Shape::Tail,
                d => Shape::Fixed(d),
            },
            (ToolAdvertising::ToolSearch, _) => Shape::Native,
        }
    }
}

impl SessionToolAdvertising {
    /// Apply a pending re-pin onto a freshly pinned session.
    pub(super) fn apply_pending(&mut self, session: &SessionId) {
        let Some(o) = self.pending.remove(session) else {
            return;
        };
        if let Some(p) = self.modes.get_mut(session) {
            p.mode = o.mode.unwrap_or(p.mode);
            p.discovery = o.discovery.unwrap_or(p.discovery);
        }
    }
}

impl AdvertisingState {
    /// The session's pinned mode (or a pending re-pin's), `None` when
    /// neither exists yet.
    pub fn session_mode(&self, session: &SessionId) -> Option<ToolAdvertising> {
        let modes = self.lock_modes();
        modes
            .get(session)
            .or_else(|| modes.pending.get(session).and_then(|o| o.mode))
    }

    /// The session's pinned discovery strategy (or a pending re-pin's) as
    /// set, whether or not the current mode/encoding applies it — see
    /// [`discovery`][Self::discovery] for the effective value.
    pub fn session_discovery(&self, session: &SessionId) -> Option<Discovery> {
        let modes = self.lock_modes();
        modes
            .get_discovery(session)
            .or_else(|| modes.pending.get(session).and_then(|o| o.discovery))
    }

    /// Replace the session's pinned mode and/or discovery strategy (`None`
    /// keeps the current value). Before the first resolution it is held and
    /// applied by the pin.
    ///
    /// When the array's shape changes: a `Full` target takes a fresh snapshot
    /// next round, and — unless the target is a fixed `invoke` array, where
    /// the set only dedups schemas already in context — the discovered set
    /// restarts. WHY restart: a name delivered under the old shape is not in
    /// the new array (an `invoke`-era tool was never appended, a `Full`-era
    /// one is gone from the kernel), yet `describe` and the arg-validate
    /// decline treat a recorded name as delivered and would never append it.
    /// Restarting makes the next delivery append it once — nothing is
    /// appended retroactively.
    pub fn repin(
        &self,
        session: &SessionId,
        mode: Option<ToolAdvertising>,
        discovery: Option<Discovery>,
    ) {
        let mut modes = self.lock_modes();
        let Some(before) = modes.modes.get(session).copied() else {
            let pending = modes.pending.entry(session.clone()).or_default();
            pending.mode = mode.or(pending.mode);
            pending.discovery = discovery.or(pending.discovery);
            return;
        };
        let after = Pinned {
            mode: mode.unwrap_or(before.mode),
            discovery: discovery.unwrap_or(before.discovery),
            ..before
        };
        modes.modes.insert(session.clone(), after);
        drop(modes);
        let (from, to) = (Shape::of(before), Shape::of(after));
        tracing::info!(session = %session.0, ?from, ?to, "tool advertising re-pinned live");
        if from == to {
            return;
        }
        self.full_surfaces
            .lock()
            .expect("full-surface mutex poisoned")
            .remove(session);
        if !matches!(to, Shape::Fixed(_)) {
            self.discovered
                .lock()
                .expect("discovered-tool mutex poisoned")
                .forget(session);
        }
    }

    fn lock_modes(&self) -> std::sync::MutexGuard<'_, SessionToolAdvertising> {
        self.modes
            .lock()
            .expect("tool-advertising mode mutex poisoned")
    }
}
