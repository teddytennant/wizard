//! The bundled Lua plugins, exercised as tools.
//!
//! One module per plugin, each behind that plugin's own cargo feature, because
//! the thing being tested is the plugin and a build without it has nothing to
//! assert. What is shared lives here: a kernel rooted in a temp directory with
//! the bundled plugins loaded into it, and the two-line call that runs a tool
//! the way the dispatcher would.
//!
//! The fixtures deliberately stop short of mocking. [`bundled_kernel`] uses the
//! real [`WizardHost`](super::super::host::WizardHost), so a behavioural test
//! runs the Lua chunk, the `ctx` table, `wizard.process.exec`, the shell runner
//! and a real process, and reads the string that comes back out the far end. A
//! mock at any layer of that would test the mock. Where a state cannot be
//! *arranged* that way — a program that exits non-zero and says nothing, one
//! that outlives its budget, a `gh` nobody has installed — the plugin's module
//! supplies a `HostBridge` whose `exec` answers from a script, which is one
//! layer further out than the Rust tests used to sit and no further.

use std::path::Path;

use serde_json::Value;

use crate::tools::{ToolContext, ToolOutput};

use super::Kernel;

/// A kernel rooted at `root`, with the bundled plugins loaded into it.
async fn bundled_kernel(root: &Path) -> Kernel {
    let kernel = super::test_kernel(root);
    super::load_into(&kernel).await;
    kernel
}

/// Call a bundled tool the way the dispatcher would.
async fn call(kernel: &Kernel, tool: &str, args: Value, cwd: &Path) -> ToolOutput {
    kernel
        .tool(tool)
        .unwrap_or_else(|| panic!("'{tool}' is registered"))
        .execute(args, &ToolContext::new(cwd))
        .await
        .expect("the tool ran")
}

#[cfg(feature = "tool-git")]
mod git;
#[cfg(feature = "tool-publish")]
mod publish;
