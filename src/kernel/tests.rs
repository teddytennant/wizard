//! Kernel tests: loading, registration, and the disposal guarantee.
//!
//! The one that matters is
//! [`a_loaded_then_unloaded_plugin_leaves_zero_residue`]. Everything else here
//! is a property that test would not have caught on its own.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use super::testing::{EchoTool, TempDir, TestPlugin, kernel_in};
use super::*;

fn tmp(tag: &str) -> TempDir {
    TempDir::new(tag)
}

/// A plugin that registers one of everything, so a disposal test has one of
/// everything to dispose.
fn kitchen_sink(name: &'static str, ran: Arc<AtomicUsize>) -> Arc<dyn Plugin> {
    TestPlugin::boxed(name, move |ctx| {
        ctx.tool(EchoTool::arc(&format!("{name}_tool")))?;
        ctx.command(PluginCommand::new(
            format!("{name}_cmd"),
            "a command",
            Arc::new(|args: String| async move { Ok(format!("ran with {args}")) }),
        ))?;
        ctx.on_fn(Event::TurnStart, 0, |_, _| async { Ok(Verdict::Continue) });
        ctx.on_fn(Event::TurnEnd, 0, |_, _| async { Ok(Verdict::Continue) });
        ctx.provide(format!("{name}_service"), Service::data(json!({"n": 1})));
        let ran = Arc::clone(&ran);
        ctx.effect("the socket", move || {
            ran.fetch_add(1, Ordering::SeqCst);
        });
        Ok(())
    })
}

#[tokio::test]
async fn a_loaded_then_unloaded_plugin_leaves_zero_residue() {
    let dir = tmp("residue");
    let kernel = kernel_in(&dir.path);
    assert!(kernel.residue().is_empty(), "a fresh kernel holds nothing");

    let torn_down = Arc::new(AtomicUsize::new(0));
    let id = kernel
        .load(kitchen_sink("sink", Arc::clone(&torn_down)))
        .expect("loads");

    // Everything landed somewhere.
    let loaded = kernel.residue();
    assert_eq!(loaded.plugins, 1);
    assert_eq!(loaded.tools, 1);
    assert_eq!(loaded.commands, 1);
    assert_eq!(loaded.handlers, 2);
    assert_eq!(loaded.services, 1);

    let report = kernel.unload(&id).await.expect("unloads");
    assert_eq!(report.plugin, "sink");
    assert_eq!(report.tools, 1);
    assert_eq!(report.commands, 1);
    assert_eq!(report.handlers, 2);
    assert_eq!(report.services, 1);
    assert_eq!(report.effects, 1);
    assert!(report.effect_failures.is_empty());
    assert_eq!(torn_down.load(Ordering::SeqCst), 1, "the teardown ran");

    // The assertion the whole kernel exists for: nothing in any registry.
    assert_eq!(
        kernel.residue(),
        Residue::default(),
        "an unloaded plugin left something behind"
    );
    assert!(kernel.tool_names().is_empty());
    assert!(kernel.command_names().is_empty());
    assert!(kernel.provider_names().is_empty());
    assert!(kernel.services().names().is_empty());
    assert!(kernel.bus().is_empty());
    assert!(kernel.loaded().is_empty());
    assert!(!kernel.is_loaded(&id));

    // And the handlers really are detached, not merely uncounted.
    assert_eq!(kernel.emit(Event::TurnStart, json!({})).await.ran, 0);
}

#[tokio::test]
async fn three_reloads_leave_the_same_kernel_as_one() {
    // The failure this guards is the one in the spec: "the third reload of a
    // plugin during a long session is a different program from the first".
    let dir = tmp("reload");
    let kernel = kernel_in(&dir.path);
    let torn_down = Arc::new(AtomicUsize::new(0));

    for _ in 0..3 {
        let plugin = kitchen_sink("sink", Arc::clone(&torn_down));
        kernel.reload(plugin).await.expect("reloads");
        let residue = kernel.residue();
        assert_eq!(residue.plugins, 1);
        assert_eq!(residue.tools, 1);
        assert_eq!(residue.handlers, 2, "handlers accumulated across reloads");
        assert_eq!(residue.services, 1);
    }
    assert_eq!(
        torn_down.load(Ordering::SeqCst),
        2,
        "two of the three unloaded"
    );

    kernel
        .unload(&PluginId::new("sink"))
        .await
        .expect("unloads");
    assert_eq!(kernel.residue(), Residue::default());
}

#[tokio::test]
async fn a_plugin_that_fails_apply_leaves_nothing_behind() {
    let dir = tmp("apply-fail");
    let kernel = kernel_in(&dir.path);
    let err = kernel
        .load(TestPlugin::boxed("half", |ctx| {
            ctx.tool(EchoTool::arc("half_tool"))?;
            ctx.provide("half_service", Service::data(json!(1)));
            ctx.on_fn(Event::TurnStart, 0, |_, _| async { Ok(Verdict::Continue) });
            anyhow::bail!("the config file was missing")
        }))
        .expect_err("apply failed");

    assert!(matches!(err, KernelError::Apply { .. }), "{err}");
    assert!(err.to_string().contains("config file"));
    assert_eq!(
        kernel.residue(),
        Residue::default(),
        "a half-applied plugin left registrations behind"
    );
    // And the name is free again.
    kernel
        .load(TestPlugin::boxed("half", |_| Ok(())))
        .expect("the name was released");
}

#[tokio::test]
async fn loading_the_same_name_twice_is_refused() {
    let dir = tmp("dup");
    let kernel = kernel_in(&dir.path);
    kernel
        .load(TestPlugin::boxed("todo", |_| Ok(())))
        .expect("first");
    let err = kernel
        .load(TestPlugin::boxed("todo", |_| Ok(())))
        .expect_err("second");
    assert!(matches!(err, KernelError::AlreadyLoaded(_)), "{err}");
    assert_eq!(kernel.residue().plugins, 1);
}

#[tokio::test]
async fn a_tool_name_belongs_to_one_plugin() {
    let dir = tmp("collide");
    let kernel = kernel_in(&dir.path);
    kernel
        .load(TestPlugin::boxed("first", |ctx| {
            ctx.tool(EchoTool::arc("shared"))?;
            Ok(())
        }))
        .expect("first");

    let err = kernel
        .load(TestPlugin::boxed("second", |ctx| {
            ctx.tool(EchoTool::arc("shared"))?;
            Ok(())
        }))
        .expect_err("the name is taken");
    let rendered = err.to_string();
    assert!(rendered.contains("shared"), "{rendered}");
    assert!(rendered.contains("first"), "{rendered}");
    assert!(rendered.contains("second"), "{rendered}");

    // The first plugin's tool survived the collision.
    assert_eq!(kernel.tool_names(), ["shared"]);
    assert_eq!(kernel.residue().plugins, 1);
}

#[tokio::test]
async fn a_command_and_a_provider_name_belong_to_one_plugin_too() {
    let dir = tmp("collide2");
    let kernel = kernel_in(&dir.path);
    let command = || {
        PluginCommand::new(
            "dupe",
            "",
            Arc::new(|_: String| async { Ok(String::new()) }),
        )
    };
    kernel
        .load(TestPlugin::boxed("a", move |ctx| {
            ctx.command(command())?;
            Ok(())
        }))
        .expect("a");
    let err = kernel
        .load(TestPlugin::boxed("b", move |ctx| {
            ctx.command(command())?;
            Ok(())
        }))
        .expect_err("taken");
    assert!(err.to_string().contains("command 'dupe'"), "{err}");
    assert_eq!(kernel.command_names(), ["dupe"]);
}

#[tokio::test]
async fn a_registered_command_runs() {
    let dir = tmp("command");
    let kernel = kernel_in(&dir.path);
    kernel
        .load(TestPlugin::boxed("greeter", |ctx| {
            ctx.command(PluginCommand::new(
                "greet",
                "says hello",
                Arc::new(|args: String| async move { Ok(format!("hello {args}")) }),
            ))?;
            Ok(())
        }))
        .expect("loads");

    let command = kernel.command("greet").expect("registered");
    assert_eq!(command.description, "says hello");
    assert_eq!(command.run("world").await.unwrap(), "hello world");
    assert!(format!("{command:?}").contains("greet"));
}

#[tokio::test]
async fn a_registered_tool_is_callable_and_exportable() {
    let dir = tmp("tools");
    let kernel = kernel_in(&dir.path);
    kernel
        .load(TestPlugin::boxed("echo", |ctx| {
            ctx.tool(EchoTool::arc("echo_b"))?;
            ctx.tool(EchoTool::arc("echo_a"))?;
            Ok(())
        }))
        .expect("loads");

    // Sorted, so a listing does not reorder itself between runs.
    assert_eq!(kernel.tool_names(), ["echo_a", "echo_b"]);
    assert_eq!(kernel.tools().len(), 2);
    assert_eq!(kernel.tools()[0].name(), "echo_a");
    assert!(kernel.tool("echo_a").is_some());
    assert!(kernel.tool("nope").is_none());

    let tool = kernel.tool("echo_a").expect("registered");
    let ctx = crate::tools::ToolContext::new(&dir.path);
    let out = tool.execute(json!({"x": 1}), &ctx).await.expect("runs");
    assert_eq!(out.content, r#"{"x":1}"#);

    // The bridge into the agent's own registry.
    let mut registry = crate::tools::registry::ToolRegistry::new();
    assert_eq!(kernel.install_tools_into(&mut registry), 2);
    assert!(registry.get("echo_a").is_some());
}

#[tokio::test]
async fn a_service_is_withdrawn_when_its_provider_unloads() {
    let dir = tmp("service");
    let kernel = kernel_in(&dir.path);
    let provider = kernel
        .load(TestPlugin::boxed("web", |ctx| {
            ctx.provide("web", Service::data(json!({"ready": true})));
            Ok(())
        }))
        .expect("provider");

    // A consumer that degrades rather than failing when the service is gone.
    let seen = Arc::new(std::sync::Mutex::new(Vec::<bool>::new()));
    let sink = Arc::clone(&seen);
    kernel
        .load(TestPlugin::boxed("summariser", move |ctx| {
            // Missing at load time is not a failure.
            assert!(ctx.inject("nothing-provides-this").is_none());
            let handle = ctx.inject_ref("web");
            let sink = Arc::clone(&sink);
            ctx.on_fn(Event::TurnStart, 0, move |_, _| {
                let present = handle.is_present();
                let sink = Arc::clone(&sink);
                async move {
                    sink.lock().unwrap().push(present);
                    Ok(Verdict::Continue)
                }
            });
            Ok(())
        }))
        .expect("consumer");

    kernel.emit(Event::TurnStart, json!({})).await;
    kernel.unload(&provider).await.expect("unload the provider");
    kernel.emit(Event::TurnStart, json!({})).await;

    assert_eq!(
        *seen.lock().unwrap(),
        [true, false],
        "the injected reference went dark when its provider unloaded"
    );
    assert!(kernel.services().is_empty());
}

#[tokio::test]
async fn a_plugin_handler_can_rewrite_and_veto() {
    let dir = tmp("verdicts");
    let kernel = kernel_in(&dir.path);
    kernel
        .load(TestPlugin::boxed("rewriter", |ctx| {
            ctx.on_fn(
                Event::PreToolUse,
                -10,
                |_, mut payload: serde_json::Value| async move {
                    payload["seen"] = json!(true);
                    Ok(Verdict::Rewrite(payload))
                },
            );
            Ok(())
        }))
        .expect("rewriter");
    kernel
        .load(TestPlugin::boxed("guard", |ctx| {
            ctx.on_fn(
                Event::PreToolUse,
                10,
                |_, payload: serde_json::Value| async move {
                    if payload["tool"] == json!("rm") {
                        Ok(Verdict::Veto("not that one".into()))
                    } else {
                        Ok(Verdict::Continue)
                    }
                },
            );
            Ok(())
        }))
        .expect("guard");

    let ok = kernel.emit(Event::PreToolUse, json!({"tool": "ls"})).await;
    assert!(!ok.is_vetoed());
    assert_eq!(ok.payload["seen"], json!(true));

    let blocked = kernel.emit(Event::PreToolUse, json!({"tool": "rm"})).await;
    assert!(blocked.is_vetoed());
    assert_eq!(blocked.veto_reason(), Some("not that one"));
    assert_eq!(blocked.veto.unwrap().plugin, "guard");
}

#[tokio::test]
async fn a_child_plugin_is_disposed_with_its_parent() {
    let dir = tmp("child");
    let kernel = kernel_in(&dir.path);
    let torn = Arc::new(AtomicUsize::new(0));
    let child_torn = Arc::clone(&torn);

    let parent = kernel
        .load(TestPlugin::boxed("parent", move |ctx| {
            ctx.tool(EchoTool::arc("parent_tool"))?;
            let child_torn = Arc::clone(&child_torn);
            ctx.plugin(
                TestPlugin::boxed("child", move |child| {
                    // The config the parent passed, not the kernel's table.
                    assert_eq!(child.config(), &json!({"from": "parent"}));
                    child.tool(EchoTool::arc("child_tool"))?;
                    child.provide("child_service", Service::data(json!(1)));
                    let torn = Arc::clone(&child_torn);
                    child.effect("child socket", move || {
                        torn.fetch_add(1, Ordering::SeqCst);
                    });
                    Ok(())
                }),
                Some(json!({"from": "parent"})),
            )?;
            Ok(())
        }))
        .expect("parent");

    assert_eq!(kernel.tool_names(), ["child_tool", "parent_tool"]);
    assert_eq!(kernel.loaded().len(), 2);

    let report = kernel.unload(&parent).await.expect("unloads");
    assert_eq!(report.children, ["child"]);
    assert_eq!(report.tools, 2, "the child's tool went too");
    assert_eq!(report.services, 1);
    assert_eq!(report.effects, 1);
    assert_eq!(torn.load(Ordering::SeqCst), 1);
    assert_eq!(kernel.residue(), Residue::default());
}

#[tokio::test]
async fn a_teardown_that_panics_does_not_stop_the_unload() {
    let dir = tmp("panic-effect");
    let kernel = kernel_in(&dir.path);
    let id = kernel
        .load(TestPlugin::boxed("brittle", |ctx| {
            ctx.tool(EchoTool::arc("brittle_tool"))?;
            ctx.effect("the bad one", || panic!("already closed"));
            Ok(())
        }))
        .expect("loads");

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let report = kernel.unload(&id).await.expect("unloads anyway");
    std::panic::set_hook(previous);

    assert_eq!(report.effect_failures, ["the bad one"]);
    assert_eq!(report.tools, 1);
    assert_eq!(kernel.residue(), Residue::default());
}

#[tokio::test]
async fn unloading_something_that_is_not_loaded_is_an_error() {
    let dir = tmp("missing");
    let kernel = kernel_in(&dir.path);
    let err = kernel
        .unload(&PluginId::new("ghost"))
        .await
        .expect_err("nothing to unload");
    assert!(matches!(err, KernelError::NotLoaded(_)), "{err}");
}

#[tokio::test]
async fn unload_all_takes_everything_newest_first() {
    let dir = tmp("all");
    let kernel = kernel_in(&dir.path);
    let order = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    for name in ["a", "b", "c"] {
        let order = Arc::clone(&order);
        kernel
            .load(TestPlugin::boxed(name, move |ctx| {
                let order = Arc::clone(&order);
                ctx.tool(EchoTool::arc(&format!("{name}_tool")))?;
                ctx.effect(name, move || order.lock().unwrap().push(name));
                Ok(())
            }))
            .expect("loads");
    }
    assert_eq!(kernel.loaded().len(), 3);

    let reports = kernel.unload_all().await;
    assert_eq!(reports.len(), 3);
    assert_eq!(*order.lock().unwrap(), ["c", "b", "a"]);
    assert_eq!(kernel.residue(), Residue::default());
    assert!(kernel.unload_all().await.is_empty());
}

#[tokio::test]
async fn a_plugin_reads_its_own_slice_of_the_config() {
    let dir = tmp("config");
    let kernel = kernel_in(&dir.path);
    kernel.set_config(json!({
        "todo": {"limit": 20},
        "web": {"allow": ["example.com"]}
    }));
    assert_eq!(kernel.config_for("todo"), json!({"limit": 20}));
    assert_eq!(kernel.config_for("absent"), Value::Null);

    kernel
        .load(TestPlugin::boxed("todo", |ctx| {
            assert_eq!(ctx.config()["limit"], json!(20));
            // A plugin with no slice reads null and falls back, rather than
            // branching on presence.
            Ok(())
        }))
        .expect("loads");
    kernel
        .load(TestPlugin::boxed("nothing", |ctx| {
            assert_eq!(ctx.config(), &Value::Null);
            Ok(())
        }))
        .expect("loads");
}

#[tokio::test]
async fn capabilities_are_visible_to_the_plugin_and_refusable() {
    let dir = tmp("caps");
    let kernel = kernel_in(&dir.path);
    kernel
        .load(TestPlugin::with_caps(
            "fetcher",
            [Capability::Network],
            |ctx| {
                assert!(ctx.has(Capability::Network));
                assert!(!ctx.has(Capability::Model));
                ctx.require(Capability::Network, "fetch a page")
                    .expect("granted");
                let err = ctx
                    .require(Capability::Model, "summarise the page")
                    .expect_err("not granted");
                let rendered = err.to_string();
                assert!(rendered.contains("fetcher"), "{rendered}");
                assert!(rendered.contains("model"), "{rendered}");
                assert!(rendered.contains("summarise the page"), "{rendered}");
                assert_eq!(ctx.capabilities().to_string(), "network");
                Ok(())
            },
        ))
        .expect("loads");

    let manifest = kernel
        .manifest_of(&PluginId::new("fetcher"))
        .expect("loaded");
    assert_eq!(manifest.capabilities, [Capability::Network]);
}

#[tokio::test]
async fn a_plugin_with_a_bad_name_never_reaches_apply() {
    let dir = tmp("bad-name");
    let kernel = kernel_in(&dir.path);
    struct Bad;
    impl Plugin for Bad {
        fn manifest(&self) -> &PluginManifest {
            // A name that cannot be a config key or a directory.
            static MANIFEST: std::sync::OnceLock<PluginManifest> = std::sync::OnceLock::new();
            MANIFEST.get_or_init(|| PluginManifest::new("not a name"))
        }
        fn apply(&self, _ctx: &mut Ctx) -> anyhow::Result<()> {
            panic!("apply must not be reached")
        }
    }
    let err = kernel.load(Arc::new(Bad)).expect_err("refused");
    assert!(matches!(err, KernelError::Manifest(_)), "{err}");
    assert_eq!(kernel.residue(), Residue::default());
}

#[tokio::test]
async fn a_plugin_emits_through_its_own_ctx() {
    let dir = tmp("ctx-emit");
    let kernel = kernel_in(&dir.path);
    let heard = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&heard);
    kernel
        .load(TestPlugin::boxed("listener", move |ctx| {
            let counter = Arc::clone(&counter);
            ctx.on_fn(Event::Checkpoint, 0, move |_, _| {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(Verdict::Continue)
                }
            });
            Ok(())
        }))
        .expect("listener");

    // A `Ctx` handed out for one plugin can emit on behalf of the whole bus,
    // which is what a plugin's async work needs.
    let ctx = kernel.context(
        &PluginId::new("emitter"),
        Arc::new(PluginManifest::new("emitter")),
        None,
    );
    let dispatch = ctx.emit(Event::Checkpoint, json!({})).await;
    assert_eq!(dispatch.ran, 1);
    assert_eq!(heard.load(Ordering::SeqCst), 1);
    assert!(format!("{ctx:?}").contains("emitter"));
}

#[tokio::test]
async fn the_unwired_host_refuses_with_a_reason_rather_than_no_opping() {
    let host = UnwiredHost;
    for err in [
        host.http("GET", "https://example.com", None)
            .await
            .unwrap_err(),
        host.model("p", "hi").await.unwrap_err(),
        host.notify("p", "hi").await.unwrap_err(),
        host.spawn_agent("p", "do it").await.unwrap_err(),
        host.run("p", "ls").await.unwrap_err(),
    ] {
        let rendered = err.to_string();
        assert!(rendered.contains("dormant"), "{rendered}");
        assert!(rendered.contains("docs/plugins.md"), "{rendered}");
    }
}

#[test]
fn residue_is_a_single_value_so_a_test_cannot_forget_a_field() {
    let empty = Residue::default();
    assert!(empty.is_empty());
    assert!(
        !Residue {
            handlers: 1,
            ..Residue::default()
        }
        .is_empty()
    );
}

#[tokio::test]
async fn a_default_kernel_is_usable_without_options() {
    // The `Default` impl reaches for `~/.wizard`, which must not be a
    // requirement for constructing one.
    let kernel = Kernel::default();
    assert!(kernel.residue().is_empty());
    assert!(kernel.plugin_root().ends_with("plugins"));
    assert_eq!(kernel.call_budget(), lua::DEFAULT_CALL_BUDGET);
    assert!(kernel.host().notify("p", "hi").await.is_err());
}

/// A provider registered through `Ctx::provider` is selectable from
/// `config.toml`, and stops being selectable the moment its plugin unloads.
///
/// This is the whole reason `ProviderKind` stopped being an enum. Before the
/// change `Ctx::provider` took a live `LlmProvider` and there was no way for a
/// `kind = "..."` to reach it — the call existed and could not be used for
/// what it was for.
#[tokio::test]
async fn a_plugin_registered_provider_is_selectable_from_config() {
    use crate::config::ProviderConfig;
    use crate::llm::registry::{self, Credentials, ProviderDescriptor, ProviderKind};

    let dir = tmp("provider-kind");
    let kernel = kernel_in(&dir.path);
    let kind = ProviderKind::known("kernel-test-backend");

    let config = ProviderConfig {
        name: "p".to_string(),
        kind: kind.clone(),
        base_url: "http://127.0.0.1:11434".to_string(),
        model: "m".to_string(),
        api_key_env: None,
        gguf_path: None,
        usd_per_mtok_in: None,
        usd_per_mtok_out: None,
    };

    // Nothing has registered it yet, so the config names a backend that is not
    // there — which loads, and fails at the point of use.
    assert!(config.descriptor().is_none());
    assert!(config.build().is_err());

    let plugin = TestPlugin::boxed("backend", move |ctx| {
        ctx.provider(ProviderDescriptor::new(
            ProviderKind::known("kernel-test-backend"),
            "A Test Backend",
            Credentials::ApiKey {
                default_env: Some("KERNEL_TEST_KEY".to_string()),
            },
            // Ollama's client needs neither a key nor a reachable endpoint,
            // which is all a registration test has to build.
            |config| {
                Ok(Arc::new(crate::llm::ollama::OllamaClient::new(
                    config.base_url.clone(),
                )))
            },
        ))?;
        Ok(())
    });
    let id = kernel.load(plugin).expect("load");

    // Both halves see it: the kernel owns it for disposal, the process
    // registry answers the config lookup.
    assert_eq!(kernel.provider_names(), ["kernel-test-backend"]);
    let descriptor = config.descriptor().expect("registered");
    assert_eq!(descriptor.display_name(), "A Test Backend");
    assert_eq!(
        descriptor.credentials().default_env(),
        Some("KERNEL_TEST_KEY")
    );
    assert!(config.build().is_ok());

    kernel.unload(&id).await.expect("unload");

    // Exact disposal reaches across the bridge: an unloaded plugin's kind is
    // no longer something a config file can select.
    assert!(kernel.provider_names().is_empty());
    assert_eq!(kernel.residue().providers, 0);
    assert!(registry::installed(&kind).is_none());
    assert!(config.build().is_err());
}

/// A plugin cannot take a kind a built-in already holds, and a refused claim
/// leaves nothing behind in either registry.
#[tokio::test]
async fn a_plugin_cannot_shadow_a_built_in_provider_kind() {
    use crate::llm::registry::{self, Credentials, ProviderDescriptor, ProviderKind};

    let dir = tmp("provider-shadow");
    let kernel = kernel_in(&dir.path);

    let err = kernel
        .load(TestPlugin::boxed("impostor", |ctx| {
            ctx.provider(ProviderDescriptor::new(
                ProviderKind::ANTHROPIC,
                "Not Anthropic",
                Credentials::Local,
                |_| Err(anyhow::anyhow!("never built")),
            ))?;
            Ok(())
        }))
        .expect_err("the built-in holds the kind");
    assert!(err.to_string().contains("anthropic"), "{err}");

    // The built-in is untouched, and the failed claim left no slot behind.
    assert!(kernel.provider_names().is_empty());
    let descriptor = registry::installed(&ProviderKind::ANTHROPIC).expect("still installed");
    assert_eq!(descriptor.display_name(), "Anthropic");
}

/// A plugin's slash command is a first-class command the moment it registers:
/// the one parser resolves it, the one dispatcher runs it, and the palette
/// lists it. And it leaves with the plugin — the same exactness the provider
/// bridge has, on the other side of the same rule.
#[tokio::test]
async fn a_plugin_registered_command_reaches_the_palette_and_leaves_with_the_plugin() {
    use crate::commands::{SlashCommand, Surface, listing, plugin};

    let dir = tmp("command-palette");
    let kernel = kernel_in(&dir.path);

    let id = kernel
        .load(TestPlugin::boxed("deployer", |ctx| {
            ctx.command(
                PluginCommand::new(
                    "zzdeploy",
                    "ship it",
                    Arc::new(|args: String| async move { Ok(format!("deploying {args}")) }),
                )
                .args("[env]"),
            )?;
            Ok(())
        }))
        .expect("loads");

    assert_eq!(
        SlashCommand::parse("/zzdeploy staging"),
        Some(Ok(SlashCommand::Plugin {
            name: "zzdeploy".to_string(),
            args: "staging".to_string(),
        }))
    );
    let row = listing(Surface::Tui)
        .into_iter()
        .find(|row| row.name == "zzdeploy")
        .expect("in the palette");
    assert_eq!(row.description, "ship it");
    assert!(row.from_plugin);
    assert_eq!(
        kernel
            .command("zzdeploy")
            .expect("in the kernel's slot too")
            .run("staging")
            .await
            .expect("runs"),
        "deploying staging"
    );

    kernel.unload(&id).await.expect("unload");

    // Exact disposal reaches across the bridge: the word is an unknown command
    // again, in the parser and in the palette, not only in the kernel.
    assert!(kernel.command_names().is_empty());
    assert_eq!(kernel.residue().commands, 0);
    assert!(plugin::get("zzdeploy").is_none());
    assert!(matches!(SlashCommand::parse("/zzdeploy"), Some(Err(_))));
}

/// A plugin cannot take a name a built-in already answers to, and a refused
/// claim leaves nothing behind in either registry. The policy itself, and why
/// it is a refusal rather than a shadow, is in `crate::commands::plugin`.
#[tokio::test]
async fn a_plugin_cannot_shadow_a_built_in_slash_command() {
    use crate::commands::{SlashCommand, plugin};

    let dir = tmp("command-shadow");
    let kernel = kernel_in(&dir.path);

    let err = kernel
        .load(TestPlugin::boxed("impostor", |ctx| {
            ctx.command(PluginCommand::new(
                "clear",
                "not the real one",
                Arc::new(|_: String| async move { Ok(String::new()) }),
            ))?;
            Ok(())
        }))
        .expect_err("the built-in holds the name");
    assert!(err.to_string().contains("clear"), "{err}");

    assert!(kernel.command_names().is_empty());
    assert!(plugin::get("clear").is_none());
    assert_eq!(SlashCommand::parse("/clear"), Some(Ok(SlashCommand::Clear)));
}

/// A failed claim does not disarm the plugin that holds the name. Two kernels
/// share one process registry, so the rollback in `insert_command` has to leave
/// the first kernel's slot and the global agreeing.
#[tokio::test]
async fn a_second_kernels_plugin_cannot_take_a_name_the_first_one_registered() {
    use crate::commands::plugin;

    let first_dir = tmp("command-first");
    let second_dir = tmp("command-second");
    let first = kernel_in(&first_dir.path);
    let second = kernel_in(&second_dir.path);

    let command = || {
        PluginCommand::new(
            "zzshared",
            "the first one",
            Arc::new(|_: String| async move { Ok("first".to_string()) }),
        )
    };
    let id = first
        .load(TestPlugin::boxed("owner", move |ctx| {
            ctx.command(command())?;
            Ok(())
        }))
        .expect("loads");

    let err = second
        .load(TestPlugin::boxed("latecomer", |ctx| {
            ctx.command(PluginCommand::new(
                "zzshared",
                "the second one",
                Arc::new(|_: String| async move { Ok("second".to_string()) }),
            ))?;
            Ok(())
        }))
        .expect_err("the first kernel's plugin holds it");
    assert!(err.to_string().contains("zzshared"), "{err}");

    assert!(second.command_names().is_empty());
    assert_eq!(
        plugin::get("zzshared")
            .expect("still the first")
            .description,
        "the first one"
    );

    first.unload(&id).await.expect("unload");
    assert!(plugin::get("zzshared").is_none());
}
