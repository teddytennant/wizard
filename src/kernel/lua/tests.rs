//! Lua plugin host tests.
//!
//! Three groups, in the order they were written: that the VM is genuinely
//! long-lived and can await (the spike's claim, now a test); that the `ctx`
//! table means the same thing it means from Rust; and that the capability
//! table in `docs/plugins.md` is enforced by absence rather than by hope.

use std::time::Duration;

use serde_json::json;

use crate::kernel::manifest::{Capability, PluginManifest, PluginSource};
use crate::kernel::testing::{RecordingHost, TempDir, TestPlugin, kernel_in, kernel_with_host};
use crate::kernel::{Event, Kernel, PluginId, Residue, Service, Verdict};

use super::load_source;

/// Load a Lua plugin from source under a manifest built from `caps`.
async fn load(
    kernel: &Kernel,
    name: &str,
    caps: &[Capability],
    source: PluginSource,
    script: &str,
) -> Result<PluginId, crate::kernel::KernelError> {
    load_source(
        kernel,
        PluginManifest::new(name).with_capabilities(caps.iter().copied()),
        source,
        script,
        &format!("@{name}.lua"),
        None,
        None,
    )
    .await
}

/// Call a plugin-registered tool.
async fn call(kernel: &Kernel, tool: &str, args: serde_json::Value) -> String {
    let tool = kernel.tool(tool).expect("the tool is registered");
    let ctx = crate::tools::ToolContext::new(kernel.project_root());
    tool.execute(args, &ctx)
        .await
        .expect("the tool ran")
        .content
}

// ---------------------------------------------------------------------------
// The VM is long-lived, and it can await.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plugin_holds_state_across_separate_tool_calls() {
    // The spec's own example, and the whole reason a plugin VM is not the
    // throwaway one `src/tools/lua.rs` gives a scripted tool.
    let dir = TempDir::new("lua-state");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "todo",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          name = "todo",
          apply = function(ctx)
            local store = {}
            ctx:tool {
              name = "todo",
              description = "remembers things",
              execute = function(args)
                if args.add then store[#store + 1] = args.add end
                return table.concat(store, ",")
              end,
            }
          end,
        }
        "#,
    )
    .await
    .expect("loads");

    assert_eq!(call(&kernel, "todo", json!({"add": "one"})).await, "one");
    assert_eq!(
        call(&kernel, "todo", json!({"add": "two"})).await,
        "one,two"
    );
    assert_eq!(call(&kernel, "todo", json!({})).await, "one,two");
}

#[tokio::test]
async fn a_plugin_awaits_in_straight_line_lua_and_keeps_state_across_the_await() {
    // "attempt to yield across C-call boundary" is the failure this rules out,
    // and holding state across the await is the second half of the claim.
    let dir = TempDir::new("lua-await");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "sleeper",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          apply = function(ctx)
            local ticks = 0
            ctx:tool {
              name = "tick",
              execute = function(args)
                local before = ticks
                for _ = 1, 3 do
                  wizard.sleep(1)
                  ticks = ticks + 1
                end
                return string.format("%d->%d", before, ticks)
              end,
            }
          end,
        }
        "#,
    )
    .await
    .expect("loads");

    assert_eq!(call(&kernel, "tick", json!({})).await, "0->3");
    assert_eq!(call(&kernel, "tick", json!({})).await, "3->6");
}

#[tokio::test]
async fn an_async_host_call_can_hand_a_table_back_into_lua() {
    let dir = TempDir::new("lua-table");
    let host = RecordingHost::arc();
    let kernel = kernel_with_host(&dir.path, host.clone(), Duration::from_secs(5));
    load(
        &kernel,
        "emitter",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          apply = function(ctx)
            ctx:on("turn_start", function(_, payload)
              return { payload = { seen = payload.n + 1 } }
            end)
            ctx:tool {
              name = "fire",
              execute = function()
                -- ctx:emit is async and returns a table; reading a field off
                -- it is the round trip this test is for.
                local result = ctx:emit("turn_start", { n = 41 })
                return tostring(result.payload.seen) .. "/" .. tostring(result.ran)
              end,
            }
          end,
        }
        "#,
    )
    .await
    .expect("loads");

    // A Lua tool emitting into a handler in its own VM: the re-entrancy the
    // FuturesUnordered loop exists for. A sequential loop deadlocks here.
    assert_eq!(call(&kernel, "fire", json!({})).await, "42/1");
}

#[tokio::test]
async fn a_bounded_plugin_is_stopped_and_its_vm_survives() {
    // The other half of the spike: a bound really fires inside `exec_async`,
    // and a VM that had one call bounded is still usable for the next one.
    let dir = TempDir::new("lua-bound");
    let kernel = kernel_with_host(&dir.path, RecordingHost::arc(), Duration::from_millis(250));
    load(
        &kernel,
        "spinner",
        &[],
        // Registry, so it is bounded and interpreted.
        PluginSource::Registry,
        r#"
        return {
          apply = function(ctx)
            ctx:tool { name = "spin", execute = function() while true do end end }
            ctx:tool { name = "quiet", execute = function() return "fine" end }
            ctx:tool {
              name = "spin_after_await",
              execute = function()
                wizard.sleep(1)
                while true do end
              end,
            }
            ctx:tool {
              name = "spin_in_pcall",
              execute = function()
                -- The stop guard: a bound is an ordinary Lua error, so a pcall
                -- would otherwise swallow it and the loop would run forever.
                while true do pcall(function() while true do end end) end
              end,
            }
          end,
        }
        "#,
    )
    .await
    .expect("loads");

    let tool_ctx = crate::tools::ToolContext::new(&dir.path);
    for name in ["spin", "spin_after_await", "spin_in_pcall"] {
        let tool = kernel.tool(name).expect("registered");
        let started = std::time::Instant::now();
        let err = tool
            .execute(json!({}), &tool_ctx)
            .await
            .expect_err("the bound fires");
        // `ToolError::Execution` renders as "tool 'x' failed"; the reason the
        // bound gives is the source, and it has to name the budget rather than
        // whatever text the plugin's own `error()` happened to use.
        let reason = std::error::Error::source(&err)
            .expect("a source")
            .to_string();
        assert!(reason.contains("compute budget"), "{name}: {reason}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{name} was not stopped promptly ({:?})",
            started.elapsed()
        );
        // And the VM still works afterwards.
        assert_eq!(
            call(&kernel, "quiet", json!({})).await,
            "fine",
            "after {name}"
        );
    }
}

#[tokio::test]
async fn an_unbounded_plugin_keeps_the_jit() {
    // First-party plugins are unbounded, which is what `jit.off()` costs and
    // what `docs/plugins.md` trades for it. `jit.status()` is the only way to
    // observe the difference from inside.
    let dir = TempDir::new("lua-jit");
    let kernel = kernel_in(&dir.path);
    for (name, source, expected) in [
        ("fast", PluginSource::FirstParty, "true"),
        ("slow", PluginSource::Registry, "false"),
    ] {
        load(
            &kernel,
            name,
            &[],
            source,
            &format!(
                r#"
                return {{
                  apply = function(ctx)
                    ctx:tool {{
                      name = "{name}_jit",
                      execute = function() return tostring(jit.status()) end,
                    }}
                  end,
                }}
                "#
            ),
        )
        .await
        .expect("loads");
        assert_eq!(
            call(&kernel, &format!("{name}_jit"), json!({})).await,
            expected,
            "{name}"
        );
    }
}

// ---------------------------------------------------------------------------
// The `ctx` table means what it means from Rust.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_lua_plugin_leaves_zero_residue_when_it_unloads() {
    let dir = TempDir::new("lua-residue");
    let kernel = kernel_in(&dir.path);
    let id = load(
        &kernel,
        "everything",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          apply = function(ctx)
            ctx:tool { name = "t", execute = function() return "t" end }
            ctx:command { name = "c", description = "c", run = function(a) return "c " .. a end }
            ctx:on("turn_start", function() end)
            ctx:on("turn_end", function() end, 5)
            ctx:provide("s", { ready = true })
            ctx:effect(function() end, "the socket")
          end,
        }
        "#,
    )
    .await
    .expect("loads");

    let loaded = kernel.residue();
    assert_eq!(loaded.tools, 1);
    assert_eq!(loaded.commands, 1);
    assert_eq!(loaded.handlers, 2);
    assert_eq!(loaded.services, 1);

    let report = kernel.unload(&id).await.expect("unloads");
    assert_eq!(report.tools, 1);
    assert_eq!(report.commands, 1);
    assert_eq!(report.handlers, 2);
    assert_eq!(report.services, 1);
    assert_eq!(report.effects, 1, "the Lua teardown ran inside its own VM");
    assert!(report.effect_failures.is_empty());
    assert_eq!(kernel.residue(), Residue::default());
}

#[tokio::test]
async fn a_lua_teardown_sees_the_plugins_own_state() {
    let dir = TempDir::new("lua-effect");
    let kernel = kernel_in(&dir.path);
    let id = load(
        &kernel,
        "closer",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          apply = function(ctx)
            local open = true
            ctx:effect(function()
              if not open then error("the socket was already closed") end
              open = false
            end, "socket")
            -- Two teardowns, so the reverse ordering is observable: this one
            -- runs first and must not find `open` already false.
            ctx:effect(function() if not open then error("out of order") end end, "check")
          end,
        }
        "#,
    )
    .await
    .expect("loads");

    let report = kernel.unload(&id).await.expect("unloads");
    assert_eq!(report.effects, 2);
    assert!(
        report.effect_failures.is_empty(),
        "{:?}",
        report.effect_failures
    );
}

#[tokio::test]
async fn a_lua_teardown_that_errors_does_not_stop_the_unload() {
    let dir = TempDir::new("lua-effect-fail");
    let kernel = kernel_in(&dir.path);
    let id = load(
        &kernel,
        "brittle",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          apply = function(ctx)
            ctx:tool { name = "t", execute = function() return "" end }
            ctx:effect(function() error("already closed") end, "bad")
            ctx:effect(function() end, "good")
          end,
        }
        "#,
    )
    .await
    .expect("loads");

    let report = kernel.unload(&id).await.expect("unloads anyway");
    assert_eq!(report.effects, 1, "the good one still ran");
    assert_eq!(report.effect_failures.len(), 1);
    assert!(report.effect_failures[0].starts_with("bad:"));
    assert_eq!(kernel.residue(), Residue::default());
}

#[tokio::test]
async fn a_lua_handler_observes_rewrites_and_vetoes() {
    let dir = TempDir::new("lua-verdicts");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "policy",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          apply = function(ctx)
            ctx:on("pre_tool_use", function(event, payload)
              if payload.tool == "rm" then return { veto = "not that one" } end
              if payload.tool == "ls" then return { payload = { tool = "ls", safe = true } } end
              -- Anything else: observe. Returning nothing means continue, and
              -- returning a bare table with neither key must not blank the
              -- payload.
              return {}
            end)
          end,
        }
        "#,
    )
    .await
    .expect("loads");

    let vetoed = kernel.emit(Event::PreToolUse, json!({"tool": "rm"})).await;
    assert_eq!(vetoed.veto_reason(), Some("not that one"));
    assert_eq!(vetoed.veto.unwrap().plugin, "policy");

    let rewritten = kernel.emit(Event::PreToolUse, json!({"tool": "ls"})).await;
    assert_eq!(rewritten.payload["safe"], json!(true));
    assert_eq!(rewritten.rewrites, 1);

    let observed = kernel.emit(Event::PreToolUse, json!({"tool": "cat"})).await;
    assert_eq!(observed.payload, json!({"tool": "cat"}));
    assert_eq!(observed.rewrites, 0);
    assert!(!observed.is_vetoed());
}

#[tokio::test]
async fn a_lua_handler_that_errors_is_skipped_rather_than_wedging_the_turn() {
    let dir = TempDir::new("lua-handler-error");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "broken",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          apply = function(ctx)
            ctx:on("turn_start", function() error("nil field") end, -1)
          end,
        }
        "#,
    )
    .await
    .expect("loads");
    kernel
        .load(TestPlugin::boxed("healthy", |ctx| {
            ctx.on_fn(Event::TurnStart, 1, |_, _| async {
                Ok(Verdict::Rewrite(json!("still ran")))
            });
            Ok(())
        }))
        .expect("loads");

    let dispatch = kernel.emit(Event::TurnStart, json!({})).await;
    assert_eq!(dispatch.payload, json!("still ran"));
    assert_eq!(dispatch.failures.len(), 1);
    assert_eq!(dispatch.failures[0].plugin, "broken");
    assert!(!dispatch.failures[0].panicked);
}

#[tokio::test]
async fn lua_and_rust_plugins_share_one_service_namespace() {
    let dir = TempDir::new("lua-services");
    let kernel = kernel_in(&dir.path);

    // A Rust plugin provides data and a native object. Lua sees the first and
    // not the second, which is the documented divergence.
    kernel
        .load(TestPlugin::boxed("rusty", |ctx| {
            ctx.provide("limits", Service::data(json!({"max": 7})));
            ctx.provide("opaque", Service::native(7_u32));
            Ok(())
        }))
        .expect("rust plugin");

    load(
        &kernel,
        "luaish",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          apply = function(ctx)
            local limits = ctx:inject("limits")
            local opaque = ctx:inject("opaque")
            local missing = ctx:inject("nobody-provides-this")
            ctx:provide("report", {
              max = limits and limits.max or -1,
              opaque_visible = opaque ~= nil,
              missing_visible = missing ~= nil,
            })
          end,
        }
        "#,
    )
    .await
    .expect("lua plugin");

    let report = kernel.services().inject("report").expect("provided");
    assert_eq!(
        report.as_data(),
        Some(&json!({
            "max": 7,
            "opaque_visible": false,
            "missing_visible": false
        }))
    );
}

#[tokio::test]
async fn a_lua_plugin_reads_its_config_slice() {
    let dir = TempDir::new("lua-config");
    let kernel = kernel_in(&dir.path);
    kernel.set_config(json!({"todo": {"limit": 3}}));
    load(
        &kernel,
        "todo",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          apply = function(ctx)
            local config = ctx:config()
            ctx:tool {
              name = "limit",
              execute = function() return tostring(config.limit) end,
            }
          end,
        }
        "#,
    )
    .await
    .expect("loads");
    assert_eq!(call(&kernel, "limit", json!({})).await, "3");
}

#[tokio::test]
async fn a_lua_command_runs() {
    let dir = TempDir::new("lua-command");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "greeter",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          apply = function(ctx)
            ctx:command {
              name = "luagreet",
              description = "says hello",
              args = "[name]",
              surfaces = { "tui" },
              run = function(args) return "hello " .. args end,
            }
          end,
        }
        "#,
    )
    .await
    .expect("loads");
    let command = kernel.command("luagreet").expect("registered");
    assert_eq!(command.description, "says hello");
    assert_eq!(command.run("world").await.unwrap(), "hello world");

    // The optional keys are the same knobs a Rust plugin has, which is the
    // point of the `Ctx` shape being identical in both languages.
    assert_eq!(command.args, "[name]");
    assert!(command.takes_args);
    assert_eq!(
        command.execution(crate::commands::Surface::Tui),
        crate::commands::Execution::Agent
    );
    assert_eq!(
        command.execution(crate::commands::Surface::Gateway),
        crate::commands::Execution::Unavailable
    );

    // And an unknown surface name is refused rather than skipped: a skipped
    // one is a command silently missing from the surface it was meant for.
    let err = load(
        &kernel,
        "typo",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          apply = function(ctx)
            ctx:command {
              name = "luatypo",
              run = function() return "" end,
              surfaces = { "terminal" },
            }
          end,
        }
        "#,
    )
    .await
    .expect_err("'terminal' is not a surface");
    assert!(err.to_string().contains("terminal"), "{err}");
}

#[tokio::test]
async fn a_lua_tool_reports_a_soft_failure_the_way_a_scripted_tool_does() {
    let dir = TempDir::new("lua-soft-error");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "failer",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          apply = function(ctx)
            ctx:tool { name = "soft", execute = function() return "error: no such branch" end }
            ctx:tool { name = "hard", execute = function() error("boom") end }
            ctx:tool { name = "quiet", execute = function() end }
            ctx:tool { name = "structured", execute = function() return { ok = true } end }
          end,
        }
        "#,
    )
    .await
    .expect("loads");

    let tool_ctx = crate::tools::ToolContext::new(&dir.path);
    let soft = kernel
        .tool("soft")
        .unwrap()
        .execute(json!({}), &tool_ctx)
        .await
        .expect("ran");
    assert!(soft.is_error);
    assert_eq!(soft.content, "error: no such branch");

    // A raised error is a `ToolError`, not a soft failure: the call could not
    // be carried out.
    let hard = kernel
        .tool("hard")
        .unwrap()
        .execute(json!({}), &tool_ctx)
        .await
        .expect_err("raised");
    assert!(hard.to_string().contains("hard"), "{hard}");

    assert_eq!(call(&kernel, "quiet", json!({})).await, "");
    assert_eq!(
        call(&kernel, "structured", json!({})).await,
        r#"{"ok":true}"#
    );
}

#[tokio::test]
async fn a_lua_tools_declared_shape_reaches_the_model() {
    let dir = TempDir::new("lua-shape");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "shaped",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          apply = function(ctx)
            ctx:tool {
              name = "look",
              description = "looks at a path",
              access = "read_only",
              parameters = { type = "object", properties = { path = { type = "string" } } },
              execute = function() return "" end,
            }
            ctx:tool { name = "bare", execute = function() return "" end }
          end,
        }
        "#,
    )
    .await
    .expect("loads");

    let look = kernel.tool("look").expect("registered");
    assert_eq!(look.description(), "looks at a path");
    assert_eq!(look.access(), crate::tools::ToolAccess::ReadOnly);
    assert_eq!(look.kind(), crate::tools::ToolKind::Scripted);
    assert_eq!(
        look.parameters()["properties"]["path"]["type"],
        json!("string")
    );

    // No `access` means the conservative answer, and no `parameters` means a
    // schema that says "no arguments" rather than null.
    let bare = kernel.tool("bare").expect("registered");
    assert_eq!(bare.access(), crate::tools::ToolAccess::Execute);
    assert_eq!(
        bare.parameters(),
        json!({"type": "object", "properties": {}})
    );
}

#[tokio::test]
async fn a_plugin_that_does_not_return_a_table_fails_to_load_cleanly() {
    let dir = TempDir::new("lua-bad");
    let kernel = kernel_in(&dir.path);
    for (name, script, needle) in [
        ("nothing", "return 7", "did not return a plugin table"),
        ("noapply", "return { name = 'x' }", "no `apply` function"),
        ("raises", "error('nope')", "did not return a plugin table"),
        (
            "applyfails",
            "return { apply = function(ctx) ctx:tool { name = 'a', execute = function() end } error('halfway') end }",
            "apply() failed",
        ),
        (
            "badevent",
            "return { apply = function(ctx) ctx:on('pre_tool', function() end) end }",
            "is not an event",
        ),
        (
            "notool",
            "return { apply = function(ctx) ctx:tool { name = 'a' } end }",
            "has no execute function",
        ),
    ] {
        let err = load(&kernel, name, &[], PluginSource::FirstParty, script)
            .await
            .err()
            .unwrap_or_else(|| panic!("{name} should have failed to load"));
        let rendered = err.to_string();
        assert!(rendered.contains(needle), "{name}: {rendered}");
    }
    assert_eq!(kernel.residue(), Residue::default());
}

#[tokio::test]
async fn a_failed_lua_load_leaves_nothing_and_frees_the_name() {
    let dir = TempDir::new("lua-fail-clean");
    let kernel = kernel_in(&dir.path);
    let err = load(
        &kernel,
        "half",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          apply = function(ctx)
            ctx:tool { name = "half_tool", execute = function() return "" end }
            ctx:provide("half_service", { a = 1 })
            ctx:on("turn_start", function() end)
            error("the config file was missing")
          end,
        }
        "#,
    )
    .await
    .expect_err("apply raised");
    assert!(err.to_string().contains("config file"), "{err}");
    assert_eq!(kernel.residue(), Residue::default());

    // The name is free again.
    load(
        &kernel,
        "half",
        &[],
        PluginSource::FirstParty,
        "return { apply = function() end }",
    )
    .await
    .expect("the name was released");
}

#[tokio::test]
async fn a_provider_cannot_be_registered_from_lua() {
    let dir = TempDir::new("lua-provider");
    let kernel = kernel_in(&dir.path);
    let err = load(
        &kernel,
        "wannabe",
        &[],
        PluginSource::FirstParty,
        "return { apply = function(ctx) ctx:provider { name = 'x' } end }",
    )
    .await
    .expect_err("refused");
    let rendered = err.to_string();
    assert!(
        rendered.contains("cannot be registered from Lua"),
        "{rendered}"
    );
    assert!(rendered.contains("docs/plugins.md"), "{rendered}");
}

#[tokio::test]
async fn a_lua_plugin_loads_from_a_directory_and_can_load_a_child() {
    let dir = TempDir::new("lua-dir");
    let kernel = kernel_in(&dir.path);
    dir.write_plugin(
        "child",
        "name = \"child\"\nversion = \"1.0.0\"\n",
        r#"
        return {
          apply = function(ctx)
            ctx:tool { name = "child_tool", execute = function() return "child" end }
            ctx:provide("child_service", { ready = true })
          end,
        }
        "#,
    );
    let parent_dir = dir.write_plugin(
        "parent",
        "name = \"parent\"\nversion = \"1.0.0\"\ndescription = \"loads a child\"\n",
        r#"
        return {
          apply = function(ctx)
            ctx:tool { name = "parent_tool", execute = function() return "parent" end }
            ctx:plugin("child", { from = "parent" })
          end,
        }
        "#,
    );

    let parent = kernel
        .load_lua(&parent_dir, PluginSource::FirstParty)
        .await
        .expect("loads");
    assert_eq!(kernel.tool_names(), ["child_tool", "parent_tool"]);
    assert_eq!(call(&kernel, "child_tool", json!({})).await, "child");
    assert_eq!(
        kernel.manifest_of(&parent).unwrap().description,
        "loads a child"
    );

    let report = kernel.unload(&parent).await.expect("unloads");
    assert_eq!(report.children, ["child"]);
    assert_eq!(report.tools, 2);
    assert_eq!(kernel.residue(), Residue::default());
}

#[tokio::test]
async fn ctx_plugin_refuses_a_path_rather_than_a_name() {
    let dir = TempDir::new("lua-escape");
    let kernel = kernel_in(&dir.path);
    let err = load(
        &kernel,
        "escaper",
        &[],
        PluginSource::FirstParty,
        "return { apply = function(ctx) ctx:plugin('../../etc') end }",
    )
    .await
    .expect_err("refused");
    assert!(err.to_string().contains("is not a plugin name"), "{err}");
}

#[tokio::test]
async fn a_missing_plugin_directory_is_a_load_failure_and_not_a_panic() {
    let dir = TempDir::new("lua-missing");
    let kernel = kernel_in(&dir.path);
    let err = kernel
        .load_lua(&dir.path.join("nope"), PluginSource::FirstParty)
        .await
        .expect_err("no such directory");
    assert!(err.to_string().contains("manifest.toml"), "{err}");

    // A directory with a manifest and no script.
    std::fs::create_dir_all(dir.path.join("half")).unwrap();
    std::fs::write(
        dir.path.join("half/manifest.toml"),
        "name = \"half\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    let err = kernel
        .load_lua(&dir.path.join("half"), PluginSource::FirstParty)
        .await
        .expect_err("no script");
    assert!(err.to_string().contains("plugin.lua"), "{err}");
    assert_eq!(kernel.residue(), Residue::default());
}

// ---------------------------------------------------------------------------
// Capabilities.
// ---------------------------------------------------------------------------

/// Load a plugin whose one tool reports what it can see, and run it.
async fn probe(kernel: &Kernel, name: &str, caps: &[Capability], expression: &str) -> String {
    load(
        kernel,
        name,
        caps,
        PluginSource::FirstParty,
        &format!(
            r#"
            return {{
              apply = function(ctx)
                ctx:tool {{
                  name = "{name}_probe",
                  execute = function() return tostring({expression}) end,
                }}
              end,
            }}
            "#
        ),
    )
    .await
    .expect("loads");
    call(kernel, &format!("{name}_probe"), json!({})).await
}

#[tokio::test]
async fn a_plugin_that_declares_nothing_gets_the_sandboxed_stdlib() {
    let dir = TempDir::new("cap-none");
    let kernel = kernel_in(&dir.path);
    assert_eq!(probe(&kernel, "a", &[], "os == nil").await, "true");
    assert_eq!(probe(&kernel, "b", &[], "io == nil").await, "true");
    assert_eq!(probe(&kernel, "c", &[], "package == nil").await, "true");
    assert_eq!(probe(&kernel, "d", &[], "require == nil").await, "true");
    assert_eq!(probe(&kernel, "e", &[], "dofile == nil").await, "true");
    // What is left is pure computation, plus the host table.
    assert_eq!(
        probe(&kernel, "f", &[], "type(string.rep)").await,
        "function"
    );
    assert_eq!(probe(&kernel, "g", &[], "wizard.runtime").await, "luajit");
}

#[tokio::test]
async fn the_capability_tables_are_absent_unless_declared() {
    let dir = TempDir::new("cap-tables");
    let host = RecordingHost::arc();
    let kernel = kernel_with_host(&dir.path, host.clone(), Duration::from_secs(5));

    // Nothing declared: every gated table is nil, and the ungated helpers are
    // there.
    assert_eq!(
        probe(
            &kernel,
            "bare",
            &[],
            "(wizard.http == nil) and (wizard.model == nil) and (wizard.ui == nil) \
             and (wizard.agent == nil) and (wizard.process == nil)"
        )
        .await,
        "true"
    );
    assert_eq!(
        probe(&kernel, "fs", &[], "type(wizard.fs.read)").await,
        "function"
    );
    assert_eq!(
        probe(&kernel, "log", &[], "type(wizard.log)").await,
        "function"
    );

    // Declared: the table is there and reaches the host.
    for (name, cap, expression) in [
        (
            "net",
            Capability::Network,
            "wizard.http.get('https://example.com')",
        ),
        ("mdl", Capability::Model, "wizard.model.complete('hi')"),
        ("agt", Capability::Agent, "wizard.agent.spawn('do it')"),
        ("prc", Capability::Process, "wizard.process.run('ls')"),
    ] {
        let answer = probe(&kernel, name, &[cap], expression).await;
        assert!(!answer.is_empty(), "{name}: {answer}");
    }
    let answer = probe(
        &kernel,
        "uix",
        &[Capability::Ui],
        "wizard.ui.notify('hi') or 'sent'",
    )
    .await;
    assert_eq!(answer, "sent");

    let calls = host.calls();
    assert!(
        calls.contains(&"http GET https://example.com".to_string()),
        "{calls:?}"
    );
    assert!(calls.contains(&"model mdl hi".to_string()), "{calls:?}");
    assert!(calls.contains(&"agent agt do it".to_string()), "{calls:?}");
    assert!(calls.contains(&"run prc ls".to_string()), "{calls:?}");
    assert!(calls.contains(&"notify uix hi".to_string()), "{calls:?}");
}

#[tokio::test]
async fn filesystem_and_process_are_narrowed_to_the_one_that_was_declared() {
    // The gap `CapabilitySet::stdlib` cannot close on its own: either
    // capability opens `os` and `io`, so the names belonging to the *other* one
    // have to be blanked.
    let dir = TempDir::new("cap-narrow");
    let kernel = kernel_in(&dir.path);

    let fs_only = [Capability::Filesystem];
    assert_eq!(
        probe(&kernel, "a", &fs_only, "type(io.open)").await,
        "function"
    );
    assert_eq!(
        probe(&kernel, "b", &fs_only, "type(os.remove)").await,
        "function"
    );
    assert_eq!(
        probe(&kernel, "c", &fs_only, "os.execute == nil").await,
        "true"
    );
    assert_eq!(
        probe(&kernel, "d", &fs_only, "os.getenv == nil").await,
        "true"
    );
    assert_eq!(
        probe(&kernel, "e", &fs_only, "io.popen == nil").await,
        "true"
    );

    let process_only = [Capability::Process];
    assert_eq!(
        probe(&kernel, "f", &process_only, "type(os.execute)").await,
        "function"
    );
    assert_eq!(
        probe(&kernel, "g", &process_only, "type(os.getenv)").await,
        "function"
    );
    assert_eq!(
        probe(&kernel, "h", &process_only, "io.open == nil").await,
        "true"
    );
    assert_eq!(
        probe(&kernel, "i", &process_only, "os.remove == nil").await,
        "true"
    );
    assert_eq!(
        probe(&kernel, "j", &process_only, "loadfile == nil").await,
        "true"
    );

    let both = [Capability::Filesystem, Capability::Process];
    assert_eq!(
        probe(&kernel, "k", &both, "type(os.execute)").await,
        "function"
    );
    assert_eq!(
        probe(&kernel, "l", &both, "type(io.open)").await,
        "function"
    );
}

#[tokio::test]
async fn the_host_file_helpers_are_confined_without_the_filesystem_capability() {
    let dir = TempDir::new("cap-confine");
    std::fs::write(dir.path.join("inside.txt"), "in the project").unwrap();
    let outside = dir.path.join("outside.txt");
    std::fs::write(&outside, "not in the project").unwrap();
    let kernel = kernel_in(&dir.path);

    // A `process`-only plugin runs under Stdlib::Full, and its host file
    // helpers are still pinned to the project: confinement follows
    // `filesystem`, not the library profile.
    let confined = [Capability::Process];
    assert_eq!(
        probe(&kernel, "a", &confined, "wizard.fs.read('inside.txt')").await,
        "in the project"
    );
    let refused = probe(
        &kernel,
        "b",
        &confined,
        &format!("select(2, pcall(wizard.fs.read, '{}'))", outside.display()),
    )
    .await;
    assert!(refused.contains("may not touch"), "{refused}");

    let granted = [Capability::Filesystem];
    assert_eq!(
        probe(
            &kernel,
            "c",
            &granted,
            &format!("wizard.fs.read('{}')", outside.display())
        )
        .await,
        "not in the project"
    );
}

#[tokio::test]
async fn a_capability_a_plugin_did_not_declare_cannot_be_reached_through_require() {
    // `require("jit")` used to be a way back to the real `jit` table --
    // `disable_jit` repoints `package.loaded.jit` for exactly that reason --
    // and this asserted the frozen table survived it.
    //
    // `narrow_stdlib` now removes `require` and `package` from every plugin,
    // because `package.loadlib` was a way back to *native code* and no
    // capability in the table means that. So the route is gone rather than
    // guarded, which is the stronger property; what is asserted here is that it
    // is really gone, and that `jit` itself is still the frozen stand-in for a
    // plugin that reaches the global directly.
    let dir = TempDir::new("cap-require");
    let kernel = kernel_with_host(&dir.path, RecordingHost::arc(), Duration::from_secs(5));
    load(
        &kernel,
        "sneaky",
        &[Capability::Process],
        PluginSource::Registry,
        r#"
        return {
          apply = function(ctx)
            ctx:tool {
              name = "peek",
              execute = function()
                return "require=" .. tostring(require ~= nil)
                  .. " package=" .. tostring(package ~= nil)
                  .. " jit.on=" .. tostring(jit.on == nil)
                  .. " jit.status=" .. tostring(jit.status())
              end,
            }
          end,
        }
        "#,
    )
    .await
    .expect("loads");
    assert_eq!(
        call(&kernel, "peek", json!({})).await,
        "require=false package=false jit.on=true jit.status=false"
    );
}

#[tokio::test]
async fn a_call_into_a_stopped_vm_fails_rather_than_hanging() {
    let dir = TempDir::new("lua-dead");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "doomed",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          apply = function(ctx)
            ctx:tool { name = "t", execute = function() return "alive" end }
          end,
        }
        "#,
    )
    .await
    .expect("loads");

    let tool = kernel.tool("t").expect("registered");
    assert_eq!(call(&kernel, "t", json!({})).await, "alive");

    // Hold the tool past the unload. The registry entry is gone, but the model
    // could be mid-turn with a call already dispatched.
    kernel
        .unload(&PluginId::new("doomed"))
        .await
        .expect("unloads");
    let tool_ctx = crate::tools::ToolContext::new(&dir.path);
    let err = tokio::time::timeout(Duration::from_secs(5), tool.execute(json!({}), &tool_ctx))
        .await
        .expect("did not hang")
        .expect_err("the VM is gone");
    assert!(err.to_string().contains("t"), "{err}");
}

#[tokio::test]
async fn a_lua_plugin_that_is_dropped_without_a_shutdown_still_stops_its_vm() {
    let dir = TempDir::new("lua-drop");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "dropped",
        &[],
        PluginSource::FirstParty,
        "return { apply = function(ctx) ctx:tool { name='t', execute=function() return 'x' end } end }",
    )
    .await
    .expect("loads");
    let tool = kernel.tool("t").expect("registered");

    // `unload_all` drops the plugin record, which aborts the VM task.
    kernel.unload_all().await;
    assert_eq!(kernel.residue(), Residue::default());

    let tool_ctx = crate::tools::ToolContext::new(&dir.path);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), tool.execute(json!({}), &tool_ctx))
            .await
            .expect("did not hang")
            .is_err()
    );
}

#[test]
fn a_lua_shutdown_reports_nothing_by_default() {
    let shutdown = super::VmShutdown::default();
    assert_eq!(shutdown.effects, 0);
    assert!(shutdown.failures.is_empty());
}

#[tokio::test]
async fn a_named_plugin_loads_from_source() {
    let dir = TempDir::new("lua-handle");
    let kernel = kernel_in(&dir.path);
    let id = load(
        &kernel,
        "named",
        &[],
        PluginSource::FirstParty,
        "return { apply = function() end }",
    )
    .await
    .expect("loads");
    assert_eq!(id.as_str(), "named");
    assert!(kernel.is_loaded(&id));
}

/// A plugin that declared only `filesystem` must not be able to load native
/// code.
///
/// `narrow_stdlib` blanks the other capability's names, but `package` was left
/// alone to match what every `Stdlib::Full` script has always had. That is
/// defensible for a locally authored tool, whose author is the user, and wrong
/// for a plugin: `package.loadlib` maps a `.so` into the process and calls it,
/// which is `ffi` wearing a different name and is not reachable from any
/// capability in the table. A plugin that asks for `filesystem` -- the mildest
/// grant a text-munging plugin needs -- would be granted arbitrary native
/// execution as a side effect, and the capability list a user reads at install
/// time would be a description of nothing.
#[tokio::test]
async fn a_capability_grant_does_not_smuggle_in_native_code_loading() {
    let dir = TempDir::new("lua-loadlib");
    let kernel = kernel_with_host(&dir.path, RecordingHost::arc(), Duration::from_secs(5));
    load(
        &kernel,
        "prober",
        &[Capability::Filesystem],
        PluginSource::Registry,
        r#"
        return {
          apply = function(ctx)
            ctx:tool {
              name = "probe",
              execute = function()
                local reach = {}
                reach[#reach+1] = "package=" .. tostring(package ~= nil)
                reach[#reach+1] = "loadlib=" ..
                  tostring(package ~= nil and package.loadlib ~= nil)
                reach[#reach+1] = "require=" .. tostring(require ~= nil)
                reach[#reach+1] = "cpath=" ..
                  tostring(package ~= nil and package.cpath ~= nil)
                return table.concat(reach, " ")
              end,
            }
          end,
        }
        "#,
    )
    .await
    .expect("the prober loads");

    let reached = call(&kernel, "probe", serde_json::json!({})).await;
    assert_eq!(
        reached, "package=false loadlib=false require=false cpath=false",
        "a filesystem grant must not reach the native-library loader: {reached}"
    );
}

/// A plugin's state belongs to the *process*, not to a session, and there is
/// currently no way for it to belong to a session.
///
/// One VM per plugin per process (see the module docs), one `LuaTool` handle
/// copied into every agent's registry, so `local store` is shared by every
/// agent alive at once — a fleet run, a gateway serving two chats, a subagent
/// and its parent. This test drives that with two [`ToolContext`]s, which is
/// what two sessions look like from a tool's side.
///
/// It is here as a *limit*, not as a feature. It is what decided that
/// `src/tools/todo.rs` stays Rust: the todo list is per-agent by construction
/// (`Agent::clear` swaps the `Arc`, and `subagent::spawn` hands a plain
/// subagent a fresh empty list precisely so its scratch todos cannot reach the
/// parent's), and a plugin cannot express any of that. Every port should ask
/// this question first: **is the state this tool keeps per-process or
/// per-session?** Per-process is Lua-shaped. Per-session is not, until a
/// plugin can hold a VM or a store per agent.
#[tokio::test]
async fn a_plugins_state_is_per_process_and_cannot_be_per_session() {
    let dir = TempDir::new("lua-session-scope");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "scratch",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          apply = function(ctx)
            local store = "(empty)"
            ctx:tool {
              name = "scratch",
              execute = function(args)
                if args.set then store = args.set end
                return store
              end,
            }
          end,
        }
        "#,
    )
    .await
    .expect("loads");

    let tool = kernel.tool("scratch").expect("registered");
    // Two sessions, each with its own tool context, exactly as two agents in
    // one process have.
    let first = crate::tools::ToolContext::new(dir.path.join("session-a"));
    let second = crate::tools::ToolContext::new(dir.path.join("session-b"));

    let wrote = tool
        .execute(serde_json::json!({ "set": "session A's work" }), &first)
        .await
        .expect("the tool ran");
    assert_eq!(wrote.content, "session A's work");

    let read = tool
        .execute(serde_json::json!({}), &second)
        .await
        .expect("the tool ran");
    assert_eq!(
        read.content, "session A's work",
        "a second session reads the first session's state; a plugin has no per-session store"
    );
}

/// What a Lua plugin hands core is a **value**, taken when `apply` ran. It is
/// never a function core can call later, and it never recomputes on read.
///
/// This is the second half of the rule the test above opens. That one asks
/// where a subsystem's *state* lives; this one asks how core *reaches* the
/// answer, and it is the question that decided `src/hardware.rs` stays Rust.
/// Machine detection has no session state at all — it is exactly the
/// "registry of things the machine has" that `docs/plugins.md` calls
/// Lua-shaped — but every core caller consults it synchronously and rarely:
/// `server::spawn` asks whether to offload to a GPU from inside an
/// `async fn`, `local_setup`'s asset picker asks behind an
/// `impl FnOnce() -> bool`, and onboarding asks from a blocking crossterm
/// loop. A Lua plugin can answer either of two ways and neither is that one.
///
/// It can publish a value, which is what this test pins — and a value is
/// computed once, at load, in every process, whether or not anybody was ever
/// going to ask. For detection that means `nvidia-smi` and `rocm-smi` on every
/// startup, for a question most sessions never put.
///
/// Or it can register a tool, which is an `async` body on a VM task — and
/// there is no synchronous door into that. `block_on` from a tokio worker is a
/// panic, and the three call sites above are on worker threads.
///
/// So: **per-process state is necessary for a Lua port and not sufficient. The
/// answer also has to be one core can await, or one worth computing before
/// anybody asks.** See `docs/plugins.md` on `hardware` and `schedule`.
#[tokio::test]
async fn a_lua_service_is_a_snapshot_and_never_something_core_can_call() {
    let dir = TempDir::new("lua-service-snapshot");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "probe",
        &[],
        PluginSource::FirstParty,
        r#"
        return {
          apply = function(ctx)
            -- Stands in for a reading off the machine: gathered here, once,
            -- because `apply` is the only place a Lua plugin can gather.
            local reading = 1
            ctx:provide("reading", { gb = reading })
            ctx:tool {
              name = "reprobe",
              execute = function()
                reading = reading + 1
                ctx:provide("reading", { gb = reading })
                return tostring(reading)
              end,
            }
          end,
        }
        "#,
    )
    .await
    .expect("loads");

    let service = kernel.services().inject("reading").expect("provided");
    assert!(
        !service.is_native(),
        "a Lua service is data; there is no object behind it for core to call"
    );
    assert_eq!(service.as_data(), Some(&json!({ "gb": 1 })));

    // Injecting again re-reads the same stored value. Nothing runs in the VM,
    // which is the whole point: a service is not a getter.
    let again = kernel.services().inject("reading").expect("still provided");
    assert_eq!(again.as_data(), Some(&json!({ "gb": 1 })));

    // The value moves only when the plugin itself runs, and the door into the
    // VM is `async`. A tool call is one the model makes; core's synchronous
    // callers have no equivalent.
    assert_eq!(call(&kernel, "reprobe", json!({})).await, "2");
    assert_eq!(
        kernel
            .services()
            .inject("reading")
            .and_then(|service| service.as_data().cloned()),
        Some(json!({ "gb": 2 })),
    );
}
