//! The `/set` dialog's Tools tab: (a) the per-session tool overlay, (b) MCP
//! server enablement — both the rows bare `/enable`'s checklist uses — and
//! (c) the session's tool-advertising mode and client-side discovery
//! strategy, which re-pin live (ADR-0196/0204).

use entanglement_provider::{Discovery, ToolAdvertising};

use super::apply::{ApplyStep, ToolsChange};
use super::ADVERTISING_PERSIST_REASON;
use crate::tui::session_tools_dialog::{overlay_diff, SessionToolRow};

/// `mcp__<server>__*` → `<server>`: a whole-server row.
pub fn server_of(name: &str) -> Option<&str> {
    name.strip_prefix("mcp__")?.strip_suffix("__*")
}

/// The one spelling the catalog, `config.yml` and every rendered row share.
pub fn discovery_label(d: Discovery) -> &'static str {
    d.label()
}

/// Section (c): the pinned advertising facts and the pending re-pin.
#[derive(Debug, Clone, PartialEq)]
pub struct AdvertisingRows {
    /// Whether the head holds the shared advertising state at all.
    available: bool,
    /// Discovery only means something on the `client_side` encoding.
    client_side: bool,
    mode: ToolAdvertising,
    initial_mode: ToolAdvertising,
    discovery: Discovery,
    initial_discovery: Discovery,
    /// The provider this session talks to — the key a persisted strategy is
    /// written under, since `discovery:` is a per-provider map. Empty until a
    /// `ModelChanged` names one, which only costs the strategy half of a save.
    provider: String,
    /// "Save as default": write the shown facts into `config.yml` for **new**
    /// sessions. Orthogonal to the live re-pin, which changes this one.
    persist: bool,
}

const DISCOVERIES: [Discovery; 3] = [Discovery::Append, Discovery::NativeFirst, Discovery::Invoke];

impl AdvertisingRows {
    pub fn new(
        available: bool,
        client_side: bool,
        mode: ToolAdvertising,
        discovery: Discovery,
        provider: String,
    ) -> Self {
        Self {
            available,
            client_side,
            mode,
            initial_mode: mode,
            discovery,
            initial_discovery: discovery,
            provider,
            persist: false,
        }
    }

    pub fn mode(&self) -> ToolAdvertising {
        self.mode
    }

    pub fn discovery(&self) -> Discovery {
        self.discovery
    }

    pub fn mode_disabled(&self) -> Option<&'static str> {
        (!self.available).then_some("advertising state isn't wired into this head")
    }

    pub fn discovery_disabled(&self) -> Option<&'static str> {
        if let Some(reason) = self.mode_disabled() {
            Some(reason)
        } else if !self.client_side {
            Some("this session's wire defers tools natively; discovery applies to client_side only")
        } else if self.mode == ToolAdvertising::Full {
            Some("only applies under tool_search")
        } else {
            None
        }
    }

    pub fn persist(&self) -> bool {
        self.persist
    }

    /// With the managed writer in place (`config::write_key`), the only thing
    /// left that can block a save is a head holding no advertising state — it
    /// then has no mode or strategy to save.
    pub fn persist_disabled(&self) -> Option<&'static str> {
        (!self.available).then_some(ADVERTISING_PERSIST_REASON)
    }

    pub fn toggle_persist(&mut self) {
        if self.persist_disabled().is_none() {
            self.persist = !self.persist;
        }
    }

    /// The config-writing step: the mode exactly as the rows show it (saving
    /// means "make what I see the default", change or no change), plus the
    /// strategy only when it is meaningful for this session and its provider
    /// is known — `discovery:` is keyed by provider name.
    pub fn persist_step(&self) -> Option<ApplyStep> {
        if !self.persist || self.persist_disabled().is_some() {
            return None;
        }
        let discovery = (self.discovery_disabled().is_none() && !self.provider.is_empty())
            .then_some(self.discovery);
        Some(ApplyStep::PersistAdvertising {
            mode: self.mode,
            provider: self.provider.clone(),
            discovery,
        })
    }

    pub fn cycle_mode(&mut self) {
        if self.mode_disabled().is_none() {
            self.mode = match self.mode {
                ToolAdvertising::Full => ToolAdvertising::ToolSearch,
                ToolAdvertising::ToolSearch => ToolAdvertising::Full,
            };
        }
    }

    pub fn cycle_discovery(&mut self, forward: bool) {
        if self.discovery_disabled().is_some() {
            return;
        }
        let i = DISCOVERIES
            .iter()
            .position(|d| *d == self.discovery)
            .unwrap_or(0);
        let len = DISCOVERIES.len();
        self.discovery = DISCOVERIES[if forward {
            (i + 1) % len
        } else {
            (i + len - 1) % len
        }];
    }

    pub fn mode_changed(&self) -> bool {
        self.mode != self.initial_mode
    }

    /// A discovery change counts only while the strategy is meaningful — a
    /// value picked before switching to `full` is not re-pinned.
    pub fn discovery_changed(&self) -> bool {
        self.discovery != self.initial_discovery && self.discovery_disabled().is_none()
    }

    pub fn repin(&self) -> Option<ApplyStep> {
        let mode = self.mode_changed().then_some(self.mode);
        let discovery = self.discovery_changed().then_some(self.discovery);
        (mode.is_some() || discovery.is_some()).then_some(ApplyStep::Repin { mode, discovery })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolsTab {
    rows: Vec<SessionToolRow>,
    initial: Vec<SessionToolRow>,
    pub adv: AdvertisingRows,
}

impl ToolsTab {
    pub fn new(rows: Vec<SessionToolRow>, adv: AdvertisingRows) -> Self {
        Self {
            initial: rows.clone(),
            rows,
            adv,
        }
    }

    pub fn rows(&self) -> &[SessionToolRow] {
        &self.rows
    }

    /// Row indices for section (a) (`servers == false`) or (b).
    pub fn indices(&self, servers: bool) -> Vec<usize> {
        (0..self.rows.len())
            .filter(|i| server_of(&self.rows[*i].name).is_some() == servers)
            .collect()
    }

    pub fn toggle(&mut self, i: usize) {
        if let Some(row) = self.rows.get_mut(i) {
            row.checked = !row.checked;
        }
    }

    /// Auto-allow only means something on an enable override row — the same
    /// rule bare `/enable`'s checklist applies.
    pub fn toggle_allow(&mut self, i: usize) {
        if let Some(row) = self.rows.get_mut(i) {
            if row.checked && !row.profile_default && server_of(&row.name).is_none() {
                row.allow = !row.allow;
            }
        }
    }

    pub fn row_changed(&self, i: usize) -> bool {
        self.rows.get(i) != self.initial.get(i)
    }

    /// The overlay/MCP step, when the rows changed.
    pub fn overlay_step(&self) -> Option<ApplyStep> {
        let changed = self.rows != self.initial;
        if !changed {
            return None;
        }
        let flipped = |on: bool| -> Vec<String> {
            self.rows
                .iter()
                .zip(&self.initial)
                .filter(|(now, was)| now.checked == on && was.checked != on)
                .filter_map(|(now, _)| server_of(&now.name).map(str::to_string))
                .collect()
        };
        Some(ApplyStep::Tools(ToolsChange {
            entries: changed.then(|| overlay_diff(&self.rows)),
            enable_servers: flipped(true),
            disable_servers: flipped(false),
        }))
    }

    pub fn pending(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .rows
            .iter()
            .zip(&self.initial)
            .filter(|(now, was)| now != was)
            .map(|(now, _)| {
                let state = match (now.checked, now.allow) {
                    (false, _) => "off",
                    (true, true) => "on (allow)",
                    (true, false) => "on",
                };
                match server_of(&now.name) {
                    Some(server) => format!("mcp {server} → {state}"),
                    None => format!("tool {} → {state}", now.name),
                }
            })
            .collect();
        if self.adv.mode_changed() {
            out.push(format!("tool advertising → {}", self.adv.mode.label()));
        }
        if self.adv.discovery_changed() {
            out.push(format!(
                "discovery → {}",
                discovery_label(self.adv.discovery)
            ));
        }
        out.extend(self.adv.persist_step().map(|s| s.label()));
        out
    }
}
