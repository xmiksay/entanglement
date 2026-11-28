//! The #934 scenario's deep tools: 300 registered [`Tool`]s in six groups,
//! each group a host script namespace backed by one envelope tool whose
//! `call_grading` names the deep tool, behind one code-built skill. Shared
//! with `fixture` (and so with the live variant).

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use entanglement_runtime::capability::Capability;
use entanglement_runtime::host_ns::{HostFn, HostNamespace};
use entanglement_runtime::skills::SkillMeta;
use entanglement_runtime::tools::CallGrading;
use entanglement_runtime::{Tool, ToolRegistry};

/// The skills, each grouping 50 tools: `<skill>_<verb>_<noun>`, called from a
/// script as `<skill>::<verb>_<noun>(#{...})`.
pub const SKILLS: [(&str, [&str; 5]); 6] = [
    (
        "billing",
        ["invoice", "payment", "refund", "subscription", "tax_rate"],
    ),
    ("crm", ["contact", "company", "deal", "note", "pipeline"]),
    (
        "inventory",
        ["stock_level", "warehouse", "sku", "reservation", "supplier"],
    ),
    (
        "shipping",
        ["shipment", "carrier", "label", "route", "parcel"],
    ),
    ("support", ["ticket", "macro", "shift", "sla", "survey"]),
    (
        "analytics",
        ["dashboard", "metric", "report", "segment", "funnel"],
    ),
];

const VERBS: [&str; 10] = [
    "get", "list", "search", "count", "export", "create", "update", "delete", "archive", "restore",
];

/// Every registered deep tool's name, in registration order.
pub fn deep_tool_names() -> Vec<String> {
    SKILLS
        .iter()
        .flat_map(|(skill, nouns)| {
            nouns.iter().flat_map(move |noun| {
                VERBS
                    .iter()
                    .map(move |verb| format!("{skill}_{verb}_{noun}"))
            })
        })
        .collect()
}

/// Every deep tool that actually ran, by name, in call order.
pub type CallLog = Arc<Mutex<Vec<String>>>;

/// One deep tool: a real registered [`Tool`] with its own description and
/// schema, never advertised.
struct DeepTool {
    name: String,
    description: String,
    read_only: bool,
    log: CallLog,
}

#[async_trait]
impl Tool for DeepTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(self.name.clone())
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The record id." },
                "sku": { "type": "string", "description": "A stock-keeping unit code." },
                "warehouse": { "type": "string", "description": "A warehouse code." },
                "fields": { "type": "object", "description": "Fields to set." }
            }
        })
    }
    fn capabilities(&self) -> &'static [Capability] {
        if self.read_only {
            &[Capability::Read]
        } else {
            &[Capability::Write]
        }
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        self.log.lock().unwrap().push(self.name.clone());
        let args: serde_json::Value = serde_json::from_str(input)?;
        Ok(serde_json::json!({ "tool": self.name, "args": args, "on_hand": 42 }).to_string())
    }
}

/// A group's host-namespace backing tool: `{"tool": "<fn>", "args": {...}}`
/// runs the registered deep tool `<skill>_<fn>`, graded as that tool.
pub struct GroupTool {
    name: String,
    skill: &'static str,
    targets: BTreeMap<String, Arc<DeepTool>>,
}

impl GroupTool {
    fn target(&self, input: &str) -> Option<(&Arc<DeepTool>, serde_json::Value)> {
        let v: serde_json::Value = serde_json::from_str(input).ok()?;
        let target = self.targets.get(v.get("tool")?.as_str()?)?;
        Some((target, v.get("args").cloned().unwrap_or_default()))
    }
}

#[async_trait]
impl Tool for GroupTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(self.name.clone())
    }
    fn description(&self) -> &str {
        "dispatch one call to a group's deep tool"
    }
    fn call_grading(&self, input: &str) -> Option<CallGrading> {
        let (target, _) = self.target(input)?;
        Some(CallGrading {
            name: target.name.clone(),
            capabilities: target.capabilities(),
        })
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        let Some((target, args)) = self.target(input) else {
            anyhow::bail!("{}: unknown function in {input}", self.skill);
        };
        target.run(&args.to_string()).await
    }
}

/// The skill grouping one namespace: its body is the function list
/// `load_skill` returns.
pub fn skill(name: &str, nouns: &[&str]) -> SkillMeta {
    let mut body = format!(
        "Call these from ONE `rhai` script as `{name}::<fn>(#{{...}})`; \
         each returns JSON text (parse_json it):\n"
    );
    for noun in nouns {
        for verb in VERBS {
            body.push_str(&format!(
                "- {name}::{verb}_{noun}(#{{id, sku, warehouse, fields}}) — {verb} a {noun}\n"
            ));
        }
    }
    let things = nouns.join(", ").replace('_', " ");
    SkillMeta::new(
        name,
        format!("{name}: {} tools over {things}", nouns.len() * VERBS.len()),
        body,
    )
}

/// Register one group's 50 deep tools; return its backing tool and namespace.
pub fn group(
    skill: &'static str,
    nouns: &[&str],
    log: &CallLog,
    reg: &mut ToolRegistry,
) -> (GroupTool, HostNamespace) {
    let mut targets = BTreeMap::new();
    for noun in nouns {
        for (i, verb) in VERBS.iter().enumerate() {
            let tool = Arc::new(DeepTool {
                name: format!("{skill}_{verb}_{noun}"),
                description: format!(
                    "{verb} a {noun} record in {skill}: id selects it; fields carries \
                     the values to set; returns the record as JSON."
                ),
                read_only: i < 5,
                log: log.clone(),
            });
            reg.register_arc(tool.clone());
            targets.insert(format!("{verb}_{noun}"), tool);
        }
    }
    let fns: Vec<HostFn> = targets
        .iter()
        .map(|(name, tool)| HostFn {
            name: name.clone(),
            description: tool.description.clone(),
        })
        .collect();
    let backing = format!("{skill}_invoke");
    let ns = HostNamespace {
        name: skill.into(),
        tool: backing.clone(),
        resolver: Arc::new(move |_| Some(fns.clone())),
    };
    (
        GroupTool {
            name: backing,
            skill,
            targets,
        },
        ns,
    )
}
