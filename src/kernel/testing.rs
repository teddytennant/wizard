//! Fixtures shared by the kernel's tests.
//!
//! A plugin is a trait with two methods and a `Ctx` is a struct with ten, so
//! almost every test here wants "a plugin that does this one thing" and would
//! otherwise be four `impl` blocks of boilerplate around one closure.
//! [`TestPlugin`] is that closure with a manifest attached.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::{Tool, ToolContext, ToolError, ToolOutput};

use super::manifest::{Capability, PluginManifest};
use super::{Ctx, HostBridge, Kernel, KernelOptions, Plugin};

/// The body of a [`TestPlugin`]'s `apply`.
type ApplyFn = dyn Fn(&Ctx) -> anyhow::Result<()> + Send + Sync;

/// A plugin whose `apply` is a closure.
pub(crate) struct TestPlugin {
    manifest: PluginManifest,
    apply: Box<ApplyFn>,
}

impl TestPlugin {
    /// Named `boxed` rather than `new` because it hands back a trait object:
    /// every caller wants one to hand straight to `Kernel::load`.
    pub(crate) fn boxed(
        name: &str,
        apply: impl Fn(&Ctx) -> anyhow::Result<()> + Send + Sync + 'static,
    ) -> Arc<dyn Plugin> {
        Arc::new(TestPlugin {
            manifest: PluginManifest::new(name),
            apply: Box::new(apply),
        })
    }

    pub(crate) fn with_caps(
        name: &str,
        caps: impl IntoIterator<Item = Capability>,
        apply: impl Fn(&Ctx) -> anyhow::Result<()> + Send + Sync + 'static,
    ) -> Arc<dyn Plugin> {
        Arc::new(TestPlugin {
            manifest: PluginManifest::new(name).with_capabilities(caps),
            apply: Box::new(apply),
        })
    }
}

impl Plugin for TestPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()> {
        (self.apply)(ctx)
    }
}

/// A tool that echoes its arguments, for tests that only care that a tool is
/// registered and callable.
pub(crate) struct EchoTool {
    pub(crate) name: String,
}

impl EchoTool {
    pub(crate) fn arc(name: &str) -> Arc<dyn Tool> {
        Arc::new(EchoTool {
            name: name.to_string(),
        })
    }
}

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "echoes its arguments"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok(args.to_string()))
    }
}

/// A [`HostBridge`] that records what it was asked for and answers with a
/// canned string.
///
/// Records rather than asserts, so a test can check both that a gated call
/// reached the host *and* that an ungated one never did.
#[derive(Default)]
pub(crate) struct RecordingHost {
    calls: Mutex<Vec<String>>,
}

impl RecordingHost {
    pub(crate) fn arc() -> Arc<RecordingHost> {
        Arc::new(RecordingHost::default())
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn record(&self, call: String) {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(call);
    }
}

#[async_trait]
impl HostBridge for RecordingHost {
    async fn http(&self, method: &str, url: &str, _body: Option<String>) -> anyhow::Result<String> {
        self.record(format!("http {method} {url}"));
        Ok(format!("body of {url}"))
    }

    async fn model(&self, plugin: &str, prompt: &str) -> anyhow::Result<String> {
        self.record(format!("model {plugin} {prompt}"));
        Ok(format!("answer to {prompt}"))
    }

    async fn notify(&self, plugin: &str, text: &str) -> anyhow::Result<()> {
        self.record(format!("notify {plugin} {text}"));
        Ok(())
    }

    async fn spawn_agent(&self, plugin: &str, task: &str) -> anyhow::Result<String> {
        self.record(format!("agent {plugin} {task}"));
        Ok(format!("did {task}"))
    }

    async fn run(&self, plugin: &str, command: &str) -> anyhow::Result<String> {
        self.record(format!("run {plugin} {command}"));
        Ok(format!("ran {command}"))
    }

    async fn exec(
        &self,
        plugin: &str,
        request: super::ExecRequest,
    ) -> anyhow::Result<super::ExecOutcome> {
        let argv = request.argv.join(" ");
        self.record(format!("exec {plugin} {argv}"));
        Ok(super::ExecOutcome {
            stdout: format!("ran {argv}"),
            code: Some(0),
            ..super::ExecOutcome::default()
        })
    }
}

/// A kernel rooted in a temp directory, so a plugin's file helpers and its
/// plugin root cannot reach anything the test did not put there.
pub(crate) fn kernel_in(root: &std::path::Path) -> Kernel {
    Kernel::new(KernelOptions {
        project_root: root.to_path_buf(),
        plugin_root: root.join("plugins"),
        ..KernelOptions::default()
    })
}

/// A kernel with a recording host and a short per-call budget, for the Lua
/// tests: the real 30-second budget would make the timeout test take half a
/// minute to prove one line.
pub(crate) fn kernel_with_host(
    root: &std::path::Path,
    host: Arc<dyn HostBridge>,
    budget: Duration,
) -> Kernel {
    Kernel::new(KernelOptions {
        project_root: root.to_path_buf(),
        plugin_root: root.join("plugins"),
        host,
        call_budget: budget,
        ..KernelOptions::default()
    })
}

/// A temp directory that removes itself.
///
/// The tree has this shape in half a dozen test modules already
/// (`src/tools/lua.rs`, `src/registry_client.rs`); it is repeated rather than
/// shared because a test helper that lives in another module's `#[cfg(test)]`
/// is not reachable from here.
pub(crate) struct TempDir {
    pub(crate) path: std::path::PathBuf,
}

impl TempDir {
    pub(crate) fn new(tag: &str) -> Self {
        let unique = format!(
            "wizard-kernel-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let path = std::env::temp_dir().join(unique.replace(['(', ')', ' '], ""));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a temp dir");
        TempDir { path }
    }

    /// Write a plugin directory: `manifest.toml` plus `plugin.lua`.
    pub(crate) fn write_plugin(
        &self,
        name: &str,
        manifest: &str,
        script: &str,
    ) -> std::path::PathBuf {
        let dir = self.path.join("plugins").join(name);
        std::fs::create_dir_all(&dir).expect("a plugin dir");
        std::fs::write(dir.join("manifest.toml"), manifest).expect("manifest.toml");
        std::fs::write(dir.join("plugin.lua"), script).expect("plugin.lua");
        dir
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
