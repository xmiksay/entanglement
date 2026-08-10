//! Single integration-test harness (ADR-0180): every former per-file test
//! binary is a module here, so the crate + deps compile and link once instead
//! of 40 times. Run one module via `cargo test -p entanglement-core --test it <mod>`.

mod common;

mod actor;
mod agent_generation_pin;
mod agent_model_pin;
mod ambiguous_stop;
mod auto_compact;
mod cache_key;
mod config_validation;
mod context_limit;
mod fresh_session_handoff;
mod generation_params;
mod hibernate;
mod history_propagation;
mod idle_ttl;
mod lazy_prompt_known_child;
mod mid_stream_error;
mod model_switch;
mod multi_user;
mod oneshot_op;
mod parallel_tools;
mod pause_semantics;
mod reasoning_block_persistence;
mod replay;
mod resume_children;
mod resume_predecessor;
mod resume_reoffer;
mod search_result_persistence;
mod seq_uniqueness;
mod session_lifecycle;
mod set_generation;
mod set_session_meta;
mod spawn_sponsored;
mod stop_semantics;
mod system_prompt_resolver;
mod tool_call_delta;
mod tool_mask;
mod tool_spec_resolver;
mod turn_loop;
mod usage;
mod wire_frame_split;
