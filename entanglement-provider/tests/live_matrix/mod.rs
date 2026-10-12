//! Support modules for the opt-in live tool-use matrix (`live_tool_matrix.rs`,
//! #826): the raw z.ai probe, the simulated MCP surface and tasks, the call
//! encodings and discovery shapes, the rhai stub executor, the per-cell agent
//! loop, and the JSONL/markdown report.

pub mod probe;
pub mod report;
pub mod rhai_exec;
pub mod runner;
pub mod tasks;
pub mod variants;
