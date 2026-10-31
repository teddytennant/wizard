//! Claude Code's dynamic workflows: a JavaScript script that fans subagents out,
//! run in the background, resumable by run id. `docs/workflows.md` is the
//! user-facing description; this is the map.
//!
//! - [`script`]: `export const meta = {...}` and the body, read without a JS engine
//! - [`schema`]: the JSON Schema subset `agent({schema})` checks and validates with
//! - [`engine`]: call numbering, the agent cap, the concurrency gate, resume and the journal
//! - [`store`]: run records, journals and named workflows on disk
//! - [`live`]: runs this process is executing, and how to stop them
//! - `vm` (feature `plugin-js`): the QuickJS side, `agent()`, `parallel()` and the rest
//! - `launch`, `runner`, `structured`: the `Workflow` tool, wired to real subagents
//! - [`command`](mod@command): `/workflows`
//!
//! Everything except `vm` and what builds on it compiles without `plugin-js`, so
//! `/workflows` can still list and show runs. Without the feature there is no
//! `Workflow` tool, because there is no engine to run a script in.

pub mod command;
pub mod engine;
pub mod live;
pub mod schema;
pub mod script;
pub mod store;

#[cfg(feature = "plugin-js")]
pub mod launch;
#[cfg(feature = "plugin-js")]
pub mod runner;
#[cfg(feature = "plugin-js")]
pub mod structured;
#[cfg(feature = "plugin-js")]
pub mod vm;

/// The reference `/workflow-authoring` prints and the `Workflow` tool points at.
pub const AUTHORING: &str = include_str!("authoring.md");

#[cfg(all(test, feature = "plugin-js"))]
mod bundled_tests;
