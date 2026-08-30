//! The registration side of the window: one [`Entrypoint`] under the name
//! `wizard gui` looks up, and nothing else.
//!
//! Kept in its own file rather than at the bottom of [`super`] because this is
//! the whole of the plugin's contract with the rest of the tree — twenty lines
//! that decide whether `wizard gui` finds a window — and burying it under
//! seventeen hundred lines of widget code is how it ends up being edited by
//! accident.
//!
//! # Why the window registers nothing else
//!
//! It has no tool, no provider and no slash command to add. Every `/name` the
//! window runs is a *built-in*, applied to a chat's live agent through
//! [`crate::plugins::gui::command`]; a plugin command registered here would be
//! a second path to the same palette. The window is a plugin because it is a
//! surface that can be left out of a build, not because it extends the ones
//! that stay in.
//!
//! # Why `apply` cannot open the window
//!
//! Plugins load from [`crate::plugins::kernel`], which is a `OnceLock`
//! initializer that runs synchronously, possibly with no tokio runtime, and is
//! reached from unit tests and from `wizard doctor`. `apply` therefore has to
//! be a handful of map inserts — the same constraint every provider plugin is
//! under. So it registers *how to start* the window and returns, and the
//! dispatch chain runs it when, and only when, the user asked for it.

use crate::entrypoint::{self, Entrypoint};
use crate::kernel::{Capability, Ctx, Plugin, PluginManifest, Service};

/// The line `wizard --help` gives `gui` **on a build that has a window**.
///
/// The one surface whose two descriptions genuinely say different things, and
/// the reason [`crate::cli::command`] switches between them rather than
/// picking one. Core's — the doc comment still on the `clap` variant — ends
/// "Needs a build with `--features native`", which is the useful sentence
/// exactly when this file is not compiled in. Printing it on a build where
/// the window is right there is core telling the reader to go and get
/// something they already have.
const ABOUT: &str = "Open the GUI: an iced window (chat list, streaming conversation, git rail) \
                     over the same agent core as the TUI. One process — no webview, no HTTP, no \
                     port. Chats are built lazily, so it opens fine without a reachable \
                     provider. See docs/native-gui.md";

/// The iced window, as a plugin.
///
/// Compiled behind `--features native`, which is the same flag the whole
/// surface has always been behind — `install.sh`'s `WIZARD_NATIVE=1`, the
/// `native` job in `.github/workflows/release.yml` and `docs/native-gui.md`
/// all name it, so it keeps the name.
pub struct NativePlugin {
    manifest: PluginManifest,
}

impl NativePlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest {
                name: "native".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: "The `wizard gui` window: an iced surface over the agent core"
                    .to_string(),
                // Everything the window does, it does in-process and
                // directly: it draws, it owns sessions, it runs the commands a
                // person types and it spends the user's tokens doing it.
                //
                // Nothing enforces this. `Capability` gates the Lua host
                // bridge and the confinement of `wizard.fs.*`, and a
                // compiled-in Rust plugin reaches past all of that into the
                // crate. So this is a declaration, not a sandbox — and it is
                // still worth writing down, because the manifest is what a
                // reader consults to find out what a plugin can touch, and a
                // plugin that declares nothing while touching everything is a
                // manifest that lies.
                capabilities: vec![
                    Capability::Filesystem,
                    Capability::Process,
                    Capability::Network,
                    Capability::Model,
                    Capability::Ui,
                    Capability::Agent,
                ],
                optional_deps: Vec::new(),
                // `full` and nothing else, which is the same statement as
                // "off by default" said in the other vocabulary: a headless
                // box, a Raspberry Pi and a CI container are three machines
                // with no display, and the stock build is the one every one of
                // them downloads. `full` is the profile for a desktop somebody
                // builds themselves.
                profiles: vec!["full".to_string()],
            },
        }
    }
}

impl Default for NativePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for NativePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()> {
        ctx.provide(
            entrypoint::GUI,
            Service::native(Entrypoint::new(entrypoint::GUI, ABOUT, super::run)),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `apply` registers the one thing it claims to, under the name core
    /// looks up. A kernel of its own rather than the process one, so this
    /// still means something in a binary where some other test already
    /// booted plugins.
    #[test]
    fn applying_the_plugin_registers_the_gui_entrypoint() {
        let kernel = crate::kernel::Kernel::new(crate::kernel::KernelOptions::default());
        kernel
            .load(std::sync::Arc::new(NativePlugin::new()))
            .expect("the window plugin loads");
        let found = kernel
            .services()
            .inject_as::<Entrypoint>(entrypoint::GUI)
            .expect("the window registered its entrypoint");
        assert_eq!(found.name(), entrypoint::GUI);
    }

    /// Unloading takes it back. The window is the first plugin whose
    /// registration is a *service* rather than a tool or a provider, so the
    /// ledger's service sweep is on this path and nowhere else in the
    /// compiled-in set.
    #[tokio::test]
    async fn unloading_the_plugin_withdraws_the_entrypoint() {
        let kernel = crate::kernel::Kernel::new(crate::kernel::KernelOptions::default());
        let id = kernel
            .load(std::sync::Arc::new(NativePlugin::new()))
            .expect("the window plugin loads");
        kernel.unload(&id).await.expect("it unloads");
        assert!(
            kernel
                .services()
                .inject_as::<Entrypoint>(entrypoint::GUI)
                .is_none(),
            "the entrypoint outlived the plugin that registered it"
        );
    }
}
