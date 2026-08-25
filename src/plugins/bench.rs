//! What the plugin architecture costs, measured on the real thing.
//!
//! `docs/plugins.md` argues that a subsystem loses nothing by becoming a
//! plugin. That is a performance claim, and until something measures it, it is
//! a hope. These are `#[ignore]`d so they never run in CI -- a timing
//! assertion on a shared machine is a flaky test -- and are run deliberately:
//!
//! ```text
//! cargo test --release --locked plugins::bench -- --ignored --nocapture
//! ```
//!
//! Release matters. A debug LuaJIT is not the LuaJIT anybody ships, and a
//! debug-profile number here would be an argument against the architecture
//! that the shipped binary does not make.
//!
//! # What is separated, and why
//!
//! Two tools that do *nothing* -- one Rust, one Lua -- isolate the bridge. The
//! difference between them is the whole cost of being a plugin: the `mlua`
//! call, the argument table, the return conversion, and the async hop back.
//! Nothing else is in the number.
//!
//! Then one tool that does real work (`git_status`, which forks git) shows the
//! same bridge cost against a realistic denominator. That second number is the
//! one that decides how much more can move, because a bridge that costs 50us
//! is free next to a 5ms fork and ruinous inside a redraw.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::kernel::testing::TempDir;
use crate::kernel::{Ctx, Kernel, Plugin, PluginManifest, PluginSource};
use crate::tools::{Tool, ToolContext, ToolError, ToolOutput};

/// Iterations per measurement. Enough that a single scheduler hiccup does not
/// move the median, few enough that the whole file runs in seconds.
const RUNS: usize = 200;

/// A tool that does nothing, in Rust. The floor: dispatch, an empty JSON
/// object in, a constant string out.
struct NullTool;

#[async_trait::async_trait]
impl Tool for NullTool {
    fn name(&self) -> &str {
        "null_rust"
    }
    fn description(&self) -> &str {
        "returns a constant"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok("ok"))
    }
}

/// The Rust half registers the way every Rust plugin does, so the comparison
/// is plugin-to-plugin rather than plugin-to-something-privileged.
struct NullPlugin(PluginManifest);

impl Plugin for NullPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.0
    }
    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()> {
        ctx.tool(Arc::new(NullTool))?;
        Ok(())
    }
}

/// The same nothing, in Lua, reached through the same dispatcher.
const NULL_LUA: &str = r#"
return {
  apply = function(ctx)
    ctx:tool {
      name = "null_lua",
      description = "returns a constant",
      execute = function() return "ok" end,
    }
  end,
}
"#;

/// Median, not mean: one descheduled iteration is real but not representative,
/// and a mean lets it rewrite the answer.
fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    let n = samples.len();
    if n % 2 == 1 {
        samples[n / 2]
    } else {
        (samples[n / 2 - 1] + samples[n / 2]) / 2
    }
}

async fn time_calls(kernel: &Kernel, tool: &str, args: Value, ctx: &ToolContext) -> Duration {
    let handle = kernel
        .tool(tool)
        .unwrap_or_else(|| panic!("'{tool}' is registered"));
    // One warm call first: the first call through a fresh VM pays for the JIT
    // and for whatever the OS has not faulted in yet, and reporting that as
    // the per-call cost would overstate it by an order of magnitude.
    let _ = handle.execute(args.clone(), ctx).await;
    let mut samples = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let start = Instant::now();
        let _ = handle.execute(args.clone(), ctx).await;
        samples.push(start.elapsed());
    }
    median(samples)
}

#[tokio::test]
#[ignore = "timing; run explicitly with --ignored --nocapture in release"]
async fn what_the_lua_bridge_costs_per_call() {
    let dir = TempDir::new("bench-bridge");
    let kernel = super::bundled::test_kernel(&dir.path);
    kernel
        .load(Arc::new(NullPlugin(PluginManifest::new("nullrust"))))
        .expect("rust plugin loads");
    crate::kernel::lua::load_source(
        &kernel,
        PluginManifest::new("nulllua"),
        PluginSource::FirstParty,
        NULL_LUA,
        "@bench-null.lua",
        None,
        None,
    )
    .await
    .expect("lua tool loads");

    let ctx = ToolContext::new(&dir.path);
    let rust = time_calls(&kernel, "null_rust", json!({}), &ctx).await;
    let lua = time_calls(&kernel, "null_lua", json!({}), &ctx).await;

    println!("\n=== per-call cost, median of {RUNS} (release) ===");
    println!("  rust tool, does nothing   {rust:>12.3?}");
    println!("  lua tool, does nothing    {lua:>12.3?}");
    println!(
        "  bridge overhead           {:>12.3?}",
        lua.saturating_sub(rust)
    );
}

#[tokio::test]
#[ignore = "timing; run explicitly with --ignored --nocapture in release"]
async fn what_loading_the_bundled_plugins_costs() {
    let dir = TempDir::new("bench-load");
    println!("\n=== startup, median of 20 ===");
    let mut samples = Vec::new();
    for _ in 0..20 {
        let start = Instant::now();
        let kernel = super::bundled::test_kernel(&dir.path);
        super::bundled::load_into(&kernel).await;
        samples.push(start.elapsed());
    }
    println!("  kernel + bundled plugins  {:>12.3?}", median(samples));
}
