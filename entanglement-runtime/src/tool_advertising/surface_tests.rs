use super::*;

fn tool_spec(name: &str) -> ToolSpec {
    ToolSpec::new(name, "d")
}

#[test]
fn full_surface_sorts_and_dedups_by_name() {
    let visible = vec![tool_spec("write"), tool_spec("read")];
    let runtime = vec![tool_spec("poll"), tool_spec("read")]; // duplicated
    let specs = super::full_surface(visible, runtime);
    let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["poll", "read", "write"]);
}

#[test]
fn mark_defer_loading_defers_everything_outside_the_kernel_even_once_discovered() {
    // A discovered tool stays deferred: its definition lives in the
    // transcript's `tool_reference` block, and flipping the flag here
    // would rewrite the cached tools prefix once per discovery.
    let mut specs = vec![
        tool_spec("read"),         // kernel
        tool_spec("bash"),         // kernel
        tool_spec("glob"),         // not kernel
        tool_spec("mcp__x__tool"), // not kernel, previously described
    ];
    super::mark_defer_loading(&mut specs);
    let deferred: Vec<(&str, bool)> = specs
        .iter()
        .map(|s| (s.name.as_str(), s.defer_loading))
        .collect();
    assert_eq!(
        deferred,
        vec![
            ("read", false),
            ("bash", false),
            ("glob", true),
            ("mcp__x__tool", true),
        ]
    );
}

#[test]
fn mark_defer_loading_keeps_at_least_the_kernel_non_deferred() {
    // Even with nothing discovered yet, the kernel alone satisfies
    // Anthropic's "at least one non-deferred tool" requirement (and is
    // harmless-but-unneeded overhead on the Responses wire, which has no
    // such requirement).
    let mut specs = vec![tool_spec("read"), tool_spec("glob"), tool_spec("grep")];
    super::mark_defer_loading(&mut specs);
    assert!(specs.iter().any(|s| !s.defer_loading));
}

// ── First-resolution pin, `Full` snapshot, enable-never-appends, re-pin ──

use std::borrow::Cow;

use async_trait::async_trait;
use entanglement_core::{Catalog, Discovery};

use super::super::AdvertisingState;
use crate::{Tool, ToolRegistry};

struct Named(&'static str);

#[async_trait]
impl Tool for Named {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(self.0)
    }
    async fn run(&self, _input: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
}

fn catalog() -> Catalog {
    serde_yaml::from_str(
        "providers:\n\
         \x20 - name: tail\n\
         \x20   default_model: t\n\
         \x20   models:\n\
         \x20     - id: t\n\
         \x20 - name: fixed\n\
         \x20   default_model: nf\n\
         \x20   discovery: native_first\n\
         \x20   models:\n\
         \x20     - id: nf\n\
         \x20     - id: fullm\n\
         \x20       tool_advertising: full\n",
    )
    .expect("valid catalog yaml")
}

fn sources() -> SurfaceSources {
    let mut reg = ToolRegistry::new();
    reg.register(Named("read"));
    reg.register(Named("glob"));
    SurfaceSources {
        tools: reg.shared(),
        avail: Arc::new(AvailableMcp::default()),
        advertising: Arc::new(AdvertisingState::new()),
        agent_specs: Vec::new(),
        inputs: Arc::new(
            AdvertisingInputs::new(
                Arc::new(crate::config::bare_config()),
                Some(Arc::new(catalog())),
            )
            .with_default_model("fixed", "nf"),
        ),
    }
}

fn bound<'a>(provider: &'a str, model: &'a str) -> SessionModel<'a> {
    SessionModel {
        provider: Some(provider),
        model: Some(model),
    }
}

fn names(specs: &[ToolSpec]) -> Vec<String> {
    specs.iter().map(|s| s.name.clone()).collect()
}

fn resolve(src: &SurfaceSources, model: SessionModel<'_>) -> Vec<ToolSpec> {
    resolve_surface(src, &SessionId::new("s"), model)
}

fn mark(src: &SurfaceSources, name: &str) {
    src.advertising
        .discovered
        .lock()
        .unwrap()
        .mark(&SessionId::new("s"), name);
}

fn register(src: &SurfaceSources, name: &'static str) {
    src.tools.write().unwrap().register(Named(name));
}

fn bytes(specs: &[ToolSpec]) -> String {
    format!("{specs:?}")
}

#[test]
fn the_first_resolution_pins_from_its_model_and_later_models_change_nothing() {
    let src = sources();
    let first = resolve(&src, bound("fixed", "nf"));
    assert!(names(&first).contains(&"invoke".to_string()));
    // A later `SetModel` onto an `append` provider keeps the pin.
    assert_eq!(bytes(&resolve(&src, bound("tail", "t"))), bytes(&first));
}

#[test]
fn an_unbound_session_pins_from_the_startup_default() {
    let src = sources();
    resolve(&src, SessionModel::default());
    let s = SessionId::new("s");
    assert_eq!(src.advertising.discovery(&s), Discovery::NativeFirst);
}

#[test]
fn full_keeps_its_start_snapshot_across_registration_and_removal() {
    let src = sources();
    let first = resolve(&src, bound("fixed", "fullm"));
    assert!(names(&first).contains(&"glob".to_string()));
    register(&src, "mcp__srv__late");
    assert_eq!(
        bytes(&resolve(&src, bound("fixed", "fullm"))),
        bytes(&first)
    );
    src.tools.write().unwrap().unregister("glob");
    assert_eq!(
        bytes(&resolve(&src, bound("fixed", "fullm"))),
        bytes(&first)
    );
}

#[test]
fn full_appends_a_delivered_schema_once_at_the_end() {
    let src = sources();
    let first = resolve(&src, bound("fixed", "fullm"));
    register(&src, "mcp__srv__late");
    mark(&src, "mcp__srv__late");
    mark(&src, "glob"); // already in the snapshot: no second copy
    let grown = resolve(&src, bound("fixed", "fullm"));
    let mut want = names(&first);
    want.push("mcp__srv__late".into());
    assert_eq!(names(&grown), want);
    assert_eq!(
        bytes(&resolve(&src, bound("fixed", "fullm"))),
        bytes(&grown)
    );
}

#[test]
fn append_mcp_enable_is_explore_visible_only_until_describe_delivers_it() {
    let src = sources();
    let s = SessionId::new("s");
    let first = resolve(&src, bound("tail", "t"));
    register(&src, "mcp__srv__t");
    src.avail.mark_enabled("srv", &s);
    assert_eq!(bytes(&resolve(&src, bound("tail", "t"))), bytes(&first));
    mark(&src, "mcp__srv__t");
    let grown = resolve(&src, bound("tail", "t"));
    let mut want = names(&first);
    want.push("mcp__srv__t".into());
    assert_eq!(names(&grown), want);
}

#[test]
fn repin_native_first_to_append_appends_nothing_retroactively() {
    let src = sources();
    let s = SessionId::new("s");
    resolve(&src, bound("fixed", "nf"));
    mark(&src, "glob"); // described under native_first
    src.advertising.repin(&s, None, Some(Discovery::Append));
    let after = names(&resolve(&src, bound("fixed", "nf")));
    assert!(!after.contains(&"invoke".to_string()), "{after:?}");
    assert!(!after.contains(&"glob".to_string()), "{after:?}");
    // The next delivery appends it, once.
    mark(&src, "glob");
    let grown = names(&resolve(&src, bound("fixed", "nf")));
    assert_eq!(grown.last().map(String::as_str), Some("glob"));
    assert_eq!(grown.iter().filter(|n| *n == "glob").count(), 1);
}

#[test]
fn repin_append_to_native_first_adds_invoke_and_keeps_the_set() {
    let src = sources();
    let s = SessionId::new("s");
    resolve(&src, bound("tail", "t"));
    mark(&src, "glob");
    assert!(names(&resolve(&src, bound("tail", "t"))).contains(&"glob".to_string()));
    src.advertising
        .repin(&s, None, Some(Discovery::NativeFirst));
    let after = names(&resolve(&src, bound("tail", "t")));
    assert_eq!(after.last().map(String::as_str), Some("invoke"));
    assert!(!after.contains(&"glob".to_string()));
    assert!(src
        .advertising
        .discovered
        .lock()
        .unwrap()
        .contains(&s, "glob"));
    assert_eq!(
        src.advertising.session_discovery(&s),
        Some(Discovery::NativeFirst)
    );
}

#[test]
fn repin_to_full_takes_a_fresh_snapshot_but_a_same_shape_repin_does_not() {
    let src = sources();
    let s = SessionId::new("s");
    let first = resolve(&src, bound("fixed", "fullm"));
    register(&src, "late");
    // Discovery is inert under `Full`: same shape, snapshot kept.
    src.advertising.repin(&s, None, Some(Discovery::Invoke));
    assert_eq!(
        bytes(&resolve(&src, bound("fixed", "fullm"))),
        bytes(&first)
    );
    src.advertising
        .repin(&s, Some(ToolAdvertising::ToolSearch), None);
    assert!(names(&resolve(&src, bound("fixed", "fullm"))).contains(&"invoke".to_string()));
    src.advertising.repin(&s, Some(ToolAdvertising::Full), None);
    assert!(names(&resolve(&src, bound("fixed", "fullm"))).contains(&"late".to_string()));
    assert_eq!(
        src.advertising.session_mode(&s),
        Some(ToolAdvertising::Full)
    );
}

#[test]
fn a_repin_before_the_first_resolution_is_applied_by_the_pin() {
    let src = sources();
    let s = SessionId::new("s");
    assert_eq!(src.advertising.session_mode(&s), None);
    src.advertising.repin(&s, Some(ToolAdvertising::Full), None);
    assert_eq!(
        src.advertising.session_mode(&s),
        Some(ToolAdvertising::Full)
    );
    assert_eq!(src.advertising.session_discovery(&s), None);
    resolve(&src, bound("fixed", "nf"));
    assert_eq!(src.advertising.mode(&s), ToolAdvertising::Full);
    assert_eq!(
        src.advertising.session_discovery(&s),
        Some(Discovery::NativeFirst)
    );
}
