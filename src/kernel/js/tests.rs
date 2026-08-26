//! JavaScript plugin host tests.
//!
//! `src/kernel/lua/tests.rs`, asked of the other engine. The three groups are
//! that module's, in the same order and for the same reasons: that the VM is
//! genuinely long-lived and can await, that the `ctx` object means what it
//! means from Rust and from Lua, and that the capability table in
//! `docs/plugins.md` is enforced by *absence* rather than by hope.
//!
//! What is not here is the half that is not about JavaScript. Disposal
//! ordering, name conflicts, the ledger and the bus are the kernel's and the
//! Lua suite already drives them through a scripted plugin; a second copy
//! would assert the kernel twice and the backend once. What is here is
//! everything that could plausibly differ between two engines, plus the two
//! places this backend is *stronger* than the Lua one — a `try` cannot swallow
//! the deadline, and a JSON round trip is exact.

use std::time::Duration;

use serde_json::json;

use crate::kernel::manifest::{Capability, PluginManifest, PluginSource};
use crate::kernel::testing::{RecordingHost, TempDir, TestPlugin, kernel_in, kernel_with_host};
use crate::kernel::{Event, Kernel, PluginId, Residue, Service, Verdict};

use super::load_source;

/// Load a JavaScript plugin from source under a manifest built from `caps`.
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
        &format!("{name}.js"),
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
    // The Lua suite's opening test, and the same claim: a plugin's VM is not
    // the throwaway one a scripted tool gets, so a `let store` in `apply` is a
    // real store that `execute` closes over.
    let dir = TempDir::new("js-state");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "todo",
        &[],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "todo",
          apply(ctx) {
            const store = [];
            ctx.tool({
              name: "todo",
              execute(args) {
                if (args.add) store.push(args.add);
                return store.join(",");
              },
            });
          },
        };
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
async fn a_plugin_awaits_in_straight_line_js_and_keeps_state_across_the_await() {
    // The half of the design that had to be proven before anything else: a
    // host call is `await`ed from ordinary-looking code, the VM survives the
    // suspension, and what the plugin was holding is still there afterwards.
    let dir = TempDir::new("js-await");
    let host = RecordingHost::arc();
    let kernel = kernel_with_host(&dir.path, host.clone(), Duration::from_secs(5));
    load(
        &kernel,
        "fetcher",
        &[Capability::Network],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "fetcher",
          apply(ctx) {
            let seen = 0;
            ctx.tool({
              name: "fetch_twice",
              async execute() {
                const first = await wizard.http.get("https://a.example");
                seen += 1;
                const second = await wizard.http.get("https://b.example");
                seen += 1;
                return `${seen}: ${first} | ${second}`;
              },
            });
          },
        };
        "#,
    )
    .await
    .expect("loads");

    assert_eq!(
        call(&kernel, "fetch_twice", json!({})).await,
        "2: body of https://a.example | body of https://b.example"
    );
    // And the counter kept climbing on the next call, which is the state half.
    assert_eq!(
        call(&kernel, "fetch_twice", json!({})).await,
        "4: body of https://a.example | body of https://b.example"
    );
    assert_eq!(host.calls().len(), 4);
}

#[tokio::test]
async fn an_async_host_call_can_hand_an_object_back_into_js() {
    // `wizard.process.exec` is the one host call that answers with structure
    // rather than a string, so it is the one that proves the return path.
    let dir = TempDir::new("js-exec");
    let host = RecordingHost::arc();
    let kernel = kernel_with_host(&dir.path, host, Duration::from_secs(5));
    load(
        &kernel,
        "runner",
        &[Capability::Process],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "runner",
          apply(ctx) {
            ctx.tool({
              name: "run_it",
              async execute() {
                const out = await wizard.process.exec({ argv: ["echo", "hi"] });
                return `${out.code}/${out.stdout}/${out.timed_out === null}`;
              },
            });
          },
        };
        "#,
    )
    .await
    .expect("loads");

    assert_eq!(
        call(&kernel, "run_it", json!({})).await,
        "0/ran echo hi/true"
    );
}

#[tokio::test]
async fn a_bounded_plugin_is_stopped_and_its_vm_survives() {
    // The Lua suite's bounding test, with a fourth spin the Lua one does not
    // have. QuickJS raises the interrupt as an *uncatchable* error, so a
    // `try`/`catch` and a `try`/`finally` cannot swallow it — where LuaJIT
    // needed `install_stop_guard` to survive a `pcall`. Both are asserted so
    // the day an engine upgrade changes that, this fails rather than the
    // sandbox quietly stopping working.
    let dir = TempDir::new("js-bound");
    let kernel = kernel_with_host(&dir.path, RecordingHost::arc(), Duration::from_millis(250));
    load(
        &kernel,
        "spinner",
        &[],
        // Registry, so it is bounded.
        PluginSource::Registry,
        r#"
        export default {
          name: "spinner",
          apply(ctx) {
            ctx.tool({ name: "spin", execute() { for (;;) {} } });
            ctx.tool({ name: "quiet", execute: () => "fine" });
            ctx.tool({
              name: "spin_after_await",
              async execute() {
                await wizard.sleep(1);
                for (;;) {}
              },
            });
            ctx.tool({
              name: "spin_in_try",
              execute() {
                for (;;) {
                  try { for (;;) {} } catch (e) { /* swallowed, and must not help */ }
                }
              },
            });
            ctx.tool({
              name: "spin_in_finally",
              execute() {
                try { for (;;) {} } finally { for (;;) {} }
              },
            });
          },
        };
        "#,
    )
    .await
    .expect("loads");

    let tool_ctx = crate::tools::ToolContext::new(&dir.path);
    for name in ["spin", "spin_after_await", "spin_in_try", "spin_in_finally"] {
        let tool = kernel.tool(name).expect("registered");
        let started = std::time::Instant::now();
        let err = tool
            .execute(json!({}), &tool_ctx)
            .await
            .expect_err("the bound fires");
        // `ToolError::Execution` renders as "tool 'x' failed"; the reason the
        // bound gives is the source, and it has to name the budget rather than
        // whatever text QuickJS put on the interrupt.
        let reason = std::error::Error::source(&err)
            .expect("a source")
            .to_string();
        assert!(reason.contains("compute budget"), "{name}: {reason}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{name} was not stopped promptly ({:?})",
            started.elapsed()
        );
        // And the VM still works afterwards, which is what un-latching the
        // stop flag when the VM goes idle buys.
        assert_eq!(
            call(&kernel, "quiet", json!({})).await,
            "fine",
            "after {name}"
        );
    }
}

#[tokio::test]
async fn an_unbounded_plugin_has_no_interrupt_handler() {
    // The other side of the bound. There is no `jit.status()` here — QuickJS
    // is an interpreter either way — so what is observable is the deadline
    // itself: a first-party plugin that computes for longer than the budget
    // finishes, and a registry one does not.
    let dir = TempDir::new("js-unbounded");
    let kernel = kernel_with_host(&dir.path, RecordingHost::arc(), Duration::from_millis(150));
    let script = r#"
        export default {
          name: "NAME",
          apply(ctx) {
            ctx.tool({
              name: "NAME_work",
              execute() {
                // Long enough to blow a 150ms budget several times over, short
                // enough that the test is not the slow one in the suite.
                const until = Date.now() + 600;
                let n = 0;
                while (Date.now() < until) n += 1;
                return "done";
              },
            });
          },
        };
    "#;

    load(
        &kernel,
        "fast",
        &[],
        PluginSource::FirstParty,
        &script.replace("NAME", "fast"),
    )
    .await
    .expect("loads");
    assert_eq!(call(&kernel, "fast_work", json!({})).await, "done");

    load(
        &kernel,
        "slow",
        &[],
        PluginSource::Registry,
        &script.replace("NAME", "slow"),
    )
    .await
    .expect("loads");
    let tool_ctx = crate::tools::ToolContext::new(&dir.path);
    let err = kernel
        .tool("slow_work")
        .expect("registered")
        .execute(json!({}), &tool_ctx)
        .await
        .expect_err("the bound fires");
    assert!(
        std::error::Error::source(&err)
            .expect("a source")
            .to_string()
            .contains("compute budget"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// The `ctx` object means what it means from Rust and from Lua.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_js_plugin_leaves_zero_residue_when_it_unloads() {
    let dir = TempDir::new("js-residue");
    let kernel = kernel_in(&dir.path);
    let id = load(
        &kernel,
        "busy",
        &[],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "busy",
          apply(ctx) {
            ctx.tool({ name: "t", execute: () => "" });
            ctx.command({ name: "jsc", run: () => "" });
            ctx.on("turn_start", () => {});
            ctx.provide("thing", { a: 1 });
            ctx.effect(() => {}, "nothing");
          },
        };
        "#,
    )
    .await
    .expect("loads");

    assert_eq!(kernel.tool_names(), vec!["t".to_string()]);
    let report = kernel.unload(&id).await.expect("unloads");
    assert_eq!(report.tools, 1);
    assert_eq!(report.commands, 1);
    assert_eq!(report.handlers, 1);
    assert_eq!(report.services, 1);
    assert_eq!(report.effects, 1);
    assert_eq!(kernel.residue(), Residue::default());
}

#[tokio::test]
async fn a_js_teardown_sees_the_plugins_own_state_and_an_error_does_not_stop_the_unload() {
    let dir = TempDir::new("js-teardown");
    let kernel = kernel_in(&dir.path);
    let id = load(
        &kernel,
        "closer",
        &[],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "closer",
          apply(ctx) {
            const open = ["socket"];
            // Registered first, so it runs *last*: teardowns are newest-first,
            // the same order the Rust and Lua backends use.
            ctx.effect(() => { open.pop(); }, "close the socket");
            ctx.effect(() => { throw new Error("no"); }, "a broken teardown");
            ctx.tool({ name: "open_count", execute: () => String(open.length) });
          },
        };
        "#,
    )
    .await
    .expect("loads");

    assert_eq!(call(&kernel, "open_count", json!({})).await, "1");
    let report = kernel.unload(&id).await.expect("unloads");
    assert_eq!(report.effects, 1, "the good one ran");
    assert_eq!(report.effect_failures.len(), 1);
    assert!(
        report.effect_failures[0].contains("a broken teardown"),
        "{:?}",
        report.effect_failures
    );
    assert_eq!(kernel.residue(), Residue::default());
}

#[tokio::test]
async fn a_js_handler_observes_rewrites_and_vetoes() {
    let dir = TempDir::new("js-verdicts");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "policy",
        &[],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "policy",
          apply(ctx) {
            ctx.on("pre_tool_use", (event, payload) => {
              if (payload.tool === "rm") return { veto: "not that one" };
              if (payload.tool === "ls") return { payload: { tool: "ls", safe: true } };
              // Anything else: observe. Returning an object with neither key
              // must not blank the payload for everything downstream.
              return {};
            });
          },
        };
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
async fn a_js_handler_that_throws_is_skipped_rather_than_wedging_the_turn() {
    let dir = TempDir::new("js-handler-error");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "broken",
        &[],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "broken",
          apply(ctx) {
            ctx.on("turn_start", () => { throw new TypeError("nope"); }, -1);
          },
        };
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
async fn all_three_backends_share_one_service_namespace() {
    let dir = TempDir::new("js-services");
    let kernel = kernel_in(&dir.path);

    // A Rust plugin provides data and a native object. JavaScript sees the
    // first and not the second, which is the same documented divergence Lua
    // has: a trait object is not a JSON value in either language.
    kernel
        .load(TestPlugin::boxed("rusty", |ctx| {
            ctx.provide("limits", Service::data(json!({"max": 7})));
            ctx.provide("opaque", Service::native(7_u32));
            Ok(())
        }))
        .expect("rust plugin");

    // And a Lua plugin's service reaches a JavaScript one, which is the claim
    // that the namespace is shared rather than merely parallel.
    crate::kernel::lua::load_source(
        &kernel,
        PluginManifest::new("luaish"),
        PluginSource::FirstParty,
        r#"return { apply = function(ctx) ctx:provide("from_lua", { hello = "lua" }) end }"#,
        "@luaish.lua",
        None,
        None,
    )
    .await
    .expect("lua plugin");

    load(
        &kernel,
        "jsish",
        &[],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "jsish",
          apply(ctx) {
            const limits = ctx.inject("limits");
            const opaque = ctx.inject("opaque");
            const missing = ctx.inject("nobody-provides-this");
            const fromLua = ctx.inject("from_lua");
            ctx.provide("report", {
              max: limits ? limits.max : -1,
              opaque_visible: opaque !== undefined,
              missing_visible: missing !== undefined,
              lua_said: fromLua ? fromLua.hello : "(nothing)",
            });
          },
        };
        "#,
    )
    .await
    .expect("js plugin");

    let report = kernel.services().inject("report").expect("provided");
    assert_eq!(
        report.as_data(),
        Some(&json!({
            "max": 7,
            "opaque_visible": false,
            "missing_visible": false,
            "lua_said": "lua"
        }))
    );
}

#[tokio::test]
async fn a_tool_can_emit_to_a_handler_in_its_own_vm() {
    // The re-entrancy the Lua module's `FuturesUnordered` loop exists for,
    // asked of the other engine, where the answer was not obvious: an
    // `AsyncContext` is behind a lock, and a naive reading says the outer call
    // holds it for the whole of its body. It does not — `WithFuture` drops the
    // guard every time it parks — so a tool that awaits `ctx.emit` releases the
    // VM for the handler that emit reaches.
    //
    // A timeout rather than a plain assertion, because the failure this guards
    // against is not a wrong answer. It is a deadlock, and a deadlocked suite
    // reads as a slow machine.
    let dir = TempDir::new("js-reentrant");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "ringer",
        &[],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "ringer",
          apply(ctx) {
            let seen = 0;
            ctx.on("turn_start", (event, payload) => {
              seen += 1;
              return { payload: { from: payload.from, seen } };
            });
            ctx.tool({
              name: "ring",
              async execute() {
                const first = await ctx.emit("turn_start", { from: "the tool" });
                const second = await ctx.emit("turn_start", { from: "the tool" });
                return `${first.payload.seen},${second.payload.seen},${first.ran}`;
              },
            });
          },
        };
        "#,
    )
    .await
    .expect("loads");

    let tool = kernel.tool("ring").expect("registered");
    let ctx = crate::tools::ToolContext::new(&dir.path);
    let out = tokio::time::timeout(Duration::from_secs(10), tool.execute(json!({}), &ctx))
        .await
        .expect("a tool emitting into its own VM must not deadlock")
        .expect("ran");
    assert_eq!(out.content, "1,2,1");
}

#[tokio::test]
async fn a_js_plugin_reads_its_config_slice() {
    let dir = TempDir::new("js-config");
    let kernel = kernel_in(&dir.path);
    kernel.set_config(json!({"todo": {"limit": 3}}));
    load(
        &kernel,
        "todo",
        &[],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "todo",
          apply(ctx) {
            const config = ctx.config();
            ctx.tool({ name: "limit", execute: () => String(config.limit) });
          },
        };
        "#,
    )
    .await
    .expect("loads");
    assert_eq!(call(&kernel, "limit", json!({})).await, "3");
}

#[tokio::test]
async fn a_js_command_runs() {
    let dir = TempDir::new("js-command");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "greeter",
        &[],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "greeter",
          apply(ctx) {
            ctx.command({
              name: "jsgreet",
              description: "says hello",
              args: "[name]",
              surfaces: ["tui"],
              run: (args) => `hello ${args}`,
            });
          },
        };
        "#,
    )
    .await
    .expect("loads");
    let command = kernel.command("jsgreet").expect("registered");
    assert_eq!(command.description, "says hello");
    assert_eq!(command.run("world").await.unwrap(), "hello world");

    // The optional keys are the same knobs a Rust or Lua plugin has, which is
    // the point of the `Ctx` shape being identical in all three.
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

    // An unknown surface name is refused rather than skipped: a skipped one is
    // a command silently missing from the surface it was meant for.
    let err = load(
        &kernel,
        "typo",
        &[],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "typo",
          apply(ctx) {
            ctx.command({ name: "jstypo", run: () => "", surfaces: ["terminal"] });
          },
        };
        "#,
    )
    .await
    .expect_err("'terminal' is not a surface");
    assert!(err.to_string().contains("terminal"), "{err}");
}

#[tokio::test]
async fn a_js_tool_reports_a_soft_failure_the_way_a_scripted_tool_does() {
    let dir = TempDir::new("js-soft-error");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "failer",
        &[],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "failer",
          apply(ctx) {
            ctx.tool({ name: "soft", execute: () => "error: no such branch" });
            ctx.tool({ name: "declared", execute: () => ({ content: "fatal: nope", is_error: true }) });
            ctx.tool({ name: "hard", execute() { throw new Error("boom"); } });
            ctx.tool({ name: "quiet", execute() {} });
            ctx.tool({ name: "structured", execute: () => ({ ok: true }) });
            ctx.tool({ name: "rejected", async execute() { throw new Error("async boom"); } });
          },
        };
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

    // The spelled-out form, which is what a ported tool needs: git's own
    // `fatal:` reaches the model verbatim and still marked as a failure.
    let declared = kernel
        .tool("declared")
        .unwrap()
        .execute(json!({}), &tool_ctx)
        .await
        .expect("ran");
    assert!(declared.is_error);
    assert_eq!(declared.content, "fatal: nope");

    // A thrown error is a `ToolError`, not a soft failure: the call could not
    // be carried out. Both spellings, because a rejected promise takes a
    // different path back through the host than a synchronous throw.
    for name in ["hard", "rejected"] {
        let err = kernel
            .tool(name)
            .unwrap()
            .execute(json!({}), &tool_ctx)
            .await
            .expect_err("threw");
        assert!(err.to_string().contains(name), "{err}");
        let reason = std::error::Error::source(&err)
            .expect("a source")
            .to_string();
        assert!(reason.contains("boom"), "{name}: {reason}");
    }

    assert_eq!(call(&kernel, "quiet", json!({})).await, "");
    assert_eq!(
        call(&kernel, "structured", json!({})).await,
        r#"{"ok":true}"#
    );
}

#[tokio::test]
async fn a_js_tools_declared_shape_reaches_the_model() {
    let dir = TempDir::new("js-shape");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "shaped",
        &[],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "shaped",
          apply(ctx) {
            ctx.tool({
              name: "look",
              description: "looks at a path",
              access: "read_only",
              parameters: { type: "object", properties: { path: { type: "string" } } },
              execute: () => "",
            });
            ctx.tool({ name: "bare", execute: () => "" });
            // The shape Lua cannot express: `properties` is an empty *object*
            // and `required` an empty *array*, and both have to survive.
            ctx.tool({
              name: "empties",
              parameters: { type: "object", properties: {}, required: [] },
              execute: () => "",
            });
          },
        };
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

    // No `object_schema` repair here, and none needed. `src/kernel/lua/host.rs`
    // carries one because mlua serialises an empty table as `[]`, and it
    // records that the mirror-image case — an empty JSON *array* — cannot be
    // written from Lua at all. Both are ordinary values in JavaScript.
    let empties = kernel.tool("empties").expect("registered");
    assert_eq!(
        empties.parameters(),
        json!({"type": "object", "properties": {}, "required": []})
    );
}

#[tokio::test]
async fn a_json_document_survives_the_round_trip_unchanged() {
    // The claim the example plugin is built on, as a test rather than as a
    // comment: what a JS plugin is handed and what it hands back are the same
    // document, empty arrays, empty objects, `null`s and all.
    let dir = TempDir::new("js-roundtrip");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "mirror",
        &[],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "mirror",
          apply(ctx) {
            ctx.tool({ name: "mirror", execute: (args) => args.document });
          },
        };
        "#,
    )
    .await
    .expect("loads");

    let document = json!({
        "empty_object": {},
        "empty_array": [],
        "null": null,
        "nested": [{"a": []}, {"b": {}}],
        "true": true,
        "int": 42,
        "float": 1.5,
        "negative": -7,
        "unicode": "héllo → 🜁",
        "": "empty key"
    });
    let tool = kernel.tool("mirror").expect("registered");
    let ctx = crate::tools::ToolContext::new(&dir.path);
    let out = tool
        .execute(json!({"document": document}), &ctx)
        .await
        .expect("ran");
    let back: serde_json::Value = serde_json::from_str(&out.content).expect("valid JSON");
    assert_eq!(back, document);
}

#[tokio::test]
async fn a_plugin_without_a_default_export_fails_to_load_cleanly() {
    let dir = TempDir::new("js-bad");
    let kernel = kernel_in(&dir.path);
    for (name, script, needle) in [
        ("nodefault", "export const x = 1;", "no default export"),
        (
            "noapply",
            "export default { name: 'x' };",
            "no `apply` function",
        ),
        ("syntax", "export default { apply( } ;", "did not parse"),
        (
            "throws",
            "throw new Error('at module scope');",
            "at module scope",
        ),
        (
            "applythrows",
            "export default { apply() { throw new Error('inside apply'); } };",
            "inside apply",
        ),
    ] {
        let err = load(&kernel, name, &[], PluginSource::FirstParty, script)
            .await
            .expect_err("does not load");
        assert!(err.to_string().contains(needle), "{name}: {err}");
    }
    // And a failed load leaves the name free and nothing registered.
    assert_eq!(kernel.residue(), Residue::default());
}

#[tokio::test]
async fn a_failed_js_load_leaves_nothing_and_frees_the_name() {
    let dir = TempDir::new("js-partial");
    let kernel = kernel_in(&dir.path);
    let err = load(
        &kernel,
        "half",
        &[],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "half",
          apply(ctx) {
            ctx.tool({ name: "registered_first", execute: () => "" });
            throw new Error("and then it failed");
          },
        };
        "#,
    )
    .await
    .expect_err("does not load");
    assert!(err.to_string().contains("and then it failed"), "{err}");
    assert_eq!(
        kernel.residue(),
        Residue::default(),
        "a tool registered before the throw is still disposed"
    );

    // The name is free, so a fixed plugin can take it.
    load(
        &kernel,
        "half",
        &[],
        PluginSource::FirstParty,
        r#"export default { name: "half", apply(ctx) { ctx.tool({ name: "t", execute: () => "ok" }); } };"#,
    )
    .await
    .expect("the second load takes the name");
    assert_eq!(call(&kernel, "t", json!({})).await, "ok");
}

#[tokio::test]
async fn a_provider_cannot_be_registered_from_js() {
    let dir = TempDir::new("js-provider");
    let kernel = kernel_in(&dir.path);
    let err = load(
        &kernel,
        "backend",
        &[],
        PluginSource::FirstParty,
        r#"export default { name: "backend", apply(ctx) { ctx.provider({ kind: "mine" }); } };"#,
    )
    .await
    .expect_err("providers stay in Rust");
    assert!(
        err.to_string()
            .contains("cannot be registered from JavaScript"),
        "{err}"
    );
    assert!(kernel.provider_names().is_empty());
}

#[tokio::test]
async fn a_js_plugin_loads_from_a_directory_and_can_load_a_child() {
    let dir = TempDir::new("js-dir");
    write_js_plugin(
        &dir,
        "child",
        "name = \"child\"\nversion = \"1.0.0\"\n",
        r#"
        export default {
          name: "child",
          apply(ctx) {
            const config = ctx.config();
            ctx.tool({ name: "child_tool", execute: () => `child says ${config.greeting}` });
          },
        };
        "#,
    );
    write_js_plugin(
        &dir,
        "parent",
        "name = \"parent\"\nversion = \"1.0.0\"\n",
        r#"
        export default {
          name: "parent",
          async apply(ctx) {
            await ctx.plugin("child", { greeting: "hello" });
          },
        };
        "#,
    );

    let kernel = kernel_in(&dir.path);
    let parent = kernel
        .load_js(
            &dir.path.join("plugins").join("parent"),
            PluginSource::FirstParty,
        )
        .await
        .expect("loads");
    assert_eq!(
        call(&kernel, "child_tool", json!({})).await,
        "child says hello"
    );

    // Unloading the parent takes the child with it, which is what makes
    // `ctx.plugin` safe to use.
    kernel.unload(&parent).await.expect("unloads");
    assert_eq!(kernel.residue(), Residue::default());
}

#[tokio::test]
async fn ctx_plugin_refuses_a_path_rather_than_a_name() {
    let dir = TempDir::new("js-escape");
    let kernel = kernel_in(&dir.path);
    let err = load(
        &kernel,
        "sneaky",
        &[],
        PluginSource::FirstParty,
        r#"export default { name: "sneaky", async apply(ctx) { await ctx.plugin("../../etc"); } };"#,
    )
    .await
    .expect_err("refuses a path");
    assert!(err.to_string().contains("not a plugin name"), "{err}");
}

#[tokio::test]
async fn a_call_into_a_stopped_vm_fails_rather_than_hanging() {
    let dir = TempDir::new("js-dead");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "doomed",
        &[],
        PluginSource::FirstParty,
        r#"export default { name: "doomed", apply(ctx) { ctx.tool({ name: "t", execute: () => "alive" }); } };"#,
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
async fn a_js_plugin_that_is_dropped_without_a_shutdown_still_stops_its_vm() {
    let dir = TempDir::new("js-drop");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "dropped",
        &[],
        PluginSource::FirstParty,
        r#"export default { name: "dropped", apply(ctx) { ctx.tool({ name: "t", execute: () => "x" }); } };"#,
    )
    .await
    .expect("loads");
    let tool = kernel.tool("t").expect("registered");

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
            export default {{
              name: "{name}",
              apply(ctx) {{
                ctx.tool({{
                  name: "{name}_probe",
                  async execute() {{ return String({expression}); }},
                }});
              }},
            }};
            "#
        ),
    )
    .await
    .expect("loads");
    call(kernel, &format!("{name}_probe"), json!({})).await
}

#[tokio::test]
async fn the_capability_tables_are_absent_unless_declared() {
    let dir = TempDir::new("js-cap-tables");
    let host = RecordingHost::arc();
    let kernel = kernel_with_host(&dir.path, host.clone(), Duration::from_secs(5));

    // Nothing declared: every gated namespace is `undefined`, and the ungated
    // helpers are there. `undefined` rather than a table that refuses is the
    // whole convention — `if (wizard.http)` has to be answerable.
    assert_eq!(
        probe(
            &kernel,
            "bare",
            &[],
            "(wizard.http === undefined) && (wizard.model === undefined) \
             && (wizard.ui === undefined) && (wizard.agent === undefined) \
             && (wizard.process === undefined) && (wizard.paths === undefined)"
        )
        .await,
        "true"
    );
    assert_eq!(
        probe(&kernel, "fs", &[], "typeof wizard.fs.read").await,
        "function"
    );
    assert_eq!(
        probe(&kernel, "log", &[], "typeof wizard.log").await,
        "function"
    );
    assert_eq!(probe(&kernel, "rt", &[], "wizard.runtime").await, "quickjs");
    assert_eq!(
        probe(&kernel, "lim", &[], "typeof wizard.limits.output").await,
        "number"
    );

    // Declared: the namespace is there and reaches the host.
    for (name, cap, expression) in [
        (
            "net",
            Capability::Network,
            "await wizard.http.get('https://example.com')",
        ),
        (
            "mdl",
            Capability::Model,
            "await wizard.model.complete('hi')",
        ),
        (
            "agt",
            Capability::Agent,
            "await wizard.agent.spawn('do it')",
        ),
        ("prc", Capability::Process, "await wizard.process.run('ls')"),
    ] {
        let answer = probe(&kernel, name, &[cap], expression).await;
        assert!(!answer.is_empty(), "{name}: {answer}");
    }
    let answer = probe(
        &kernel,
        "uix",
        &[Capability::Ui],
        "(await wizard.ui.notify('hi'), 'sent')",
    )
    .await;
    assert_eq!(answer, "sent");
    assert_eq!(
        probe(
            &kernel,
            "pth",
            &[Capability::Filesystem],
            "typeof wizard.paths.project"
        )
        .await,
        "string"
    );

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
async fn the_host_file_helpers_are_confined_without_the_filesystem_capability() {
    let dir = TempDir::new("js-confine");
    std::fs::write(dir.path.join("inside.txt"), "in the project").expect("a file");
    let kernel = kernel_in(&dir.path);

    // Inside the project: fine, with or without the grant.
    assert_eq!(
        probe(&kernel, "a", &[], "wizard.fs.read('inside.txt')").await,
        "in the project"
    );

    // Outside it: refused, and the refusal names the reason rather than
    // silently re-rooting the path under the project.
    for (name, path, needle) in [
        ("b", "/etc/hostname", "absolute paths"),
        ("c", "../escape.txt", "climbs out"),
        ("d", "~/.ssh/id_rsa", "home-relative"),
    ] {
        let answer = probe(
            &kernel,
            name,
            &[],
            &format!("(() => {{ try {{ return wizard.fs.read('{path}'); }} catch (e) {{ return e.message; }} }})()"),
        )
        .await;
        assert!(answer.contains(needle), "{name}: {answer}");
    }
}

#[tokio::test]
async fn a_capability_grant_does_not_smuggle_in_native_code_loading() {
    // The JavaScript half of
    // `lua::tests::a_capability_grant_does_not_smuggle_in_native_code_loading`.
    //
    // There, the escape was `package.loadlib`, and the fix was removing
    // `package` and `require` from every plugin. Here the equivalent is the
    // module loader, and the fix is upstream of the VM: rquickjs's `loader`
    // and `dyn-load` cargo features are not enabled and nothing calls
    // `set_loader`, so `import` has nothing to resolve against. This asserts
    // that from inside — a plugin holding the mildest grant there is still
    // cannot reach code outside its own file, statically or dynamically.
    let dir = TempDir::new("js-loadlib");
    let kernel = kernel_with_host(&dir.path, RecordingHost::arc(), Duration::from_secs(5));
    load(
        &kernel,
        "prober",
        &[Capability::Filesystem],
        PluginSource::Registry,
        r#"
        export default {
          name: "prober",
          apply(ctx) {
            ctx.tool({
              name: "probe",
              async execute() {
                const reach = [];
                reach.push("require=" + (typeof require));
                reach.push("process=" + (typeof process));
                reach.push("Atomics=" + (typeof Atomics));
                reach.push("SharedArrayBuffer=" + (typeof SharedArrayBuffer));
                reach.push("FinalizationRegistry=" + (typeof FinalizationRegistry));
                try {
                  await import("os");
                  reach.push("import=RESOLVED");
                } catch (e) {
                  reach.push("import=refused");
                }
                return reach.join(" ");
              },
            });
          },
        };
        "#,
    )
    .await
    .expect("the prober loads");

    let reached = call(&kernel, "probe", json!({})).await;
    assert_eq!(
        reached,
        "require=undefined process=undefined Atomics=undefined \
         SharedArrayBuffer=undefined FinalizationRegistry=undefined import=refused",
        "a filesystem grant must not reach code outside the plugin's own file: {reached}"
    );
}

#[tokio::test]
async fn a_static_import_is_refused_at_load_rather_than_at_first_use() {
    // The other half of the loader story. A dynamic `import()` rejects a
    // promise the plugin can catch; a static one has to fail the *load*, or a
    // plugin with an unreachable dependency would register its tools and then
    // fail in front of the model.
    let dir = TempDir::new("js-static-import");
    let kernel = kernel_in(&dir.path);
    let err = load(
        &kernel,
        "importer",
        &[Capability::Filesystem],
        PluginSource::Registry,
        r#"
        import * as os from "os";
        export default { name: "importer", apply(ctx) { ctx.tool({ name: "t", execute: () => os }); } };
        "#,
    )
    .await
    .expect_err("there is no loader");
    assert!(
        err.to_string().contains("importer.js"),
        "the failure should name the file: {err}"
    );
    assert_eq!(kernel.residue(), Residue::default());
}

#[tokio::test]
async fn a_plugins_state_is_per_process_and_cannot_be_per_session() {
    // The limit that decided `src/tools/todo.rs` stays Rust, restated for the
    // third backend so the next port finds it here too. One VM per plugin per
    // process, one tool handle copied into every agent's registry, so a
    // plugin's `let` is shared by every agent alive at once.
    //
    // It is here as a *limit*, not a feature. Nothing about JavaScript changes
    // it: what would fix it is a VM per agent, which is a kernel change with a
    // real cost.
    let dir = TempDir::new("js-session-scope");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "scratch",
        &[],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "scratch",
          apply(ctx) {
            let store = "(empty)";
            ctx.tool({
              name: "scratch",
              execute(args) {
                if (args.set) store = args.set;
                return store;
              },
            });
          },
        };
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
        .execute(json!({ "set": "session A's work" }), &first)
        .await
        .expect("the tool ran");
    assert_eq!(wrote.content, "session A's work");

    let read = tool
        .execute(json!({}), &second)
        .await
        .expect("the tool ran");
    assert_eq!(
        read.content, "session A's work",
        "a second session reads the first session's state; a plugin has no per-session store"
    );
}

#[tokio::test]
async fn a_tool_body_is_told_the_directory_the_call_is_about() {
    let dir = TempDir::new("js-cwd");
    let kernel = kernel_in(&dir.path);
    load(
        &kernel,
        "where",
        &[],
        PluginSource::FirstParty,
        r#"
        export default {
          name: "where",
          apply(ctx) {
            ctx.tool({ name: "where", execute: (_args, call) => call.cwd });
          },
        };
        "#,
    )
    .await
    .expect("loads");

    let tool = kernel.tool("where").expect("registered");
    let elsewhere = dir.path.join("somewhere-else");
    let out = tool
        .execute(json!({}), &crate::tools::ToolContext::new(&elsewhere))
        .await
        .expect("ran");
    assert_eq!(out.content, elsewhere.to_string_lossy());
}

/// Write a `manifest.toml` + `plugin.js` pair under the kernel's plugin root.
///
/// [`TempDir::write_plugin`] writes `plugin.lua`; this is its sibling, and the
/// two are separate rather than parameterised because the file name is exactly
/// what decides which backend loads a directory (see
/// `crate::plugins::load_user_plugins`), so a helper that took the extension
/// as an argument would hide the one thing these tests are about.
fn write_js_plugin(dir: &TempDir, name: &str, manifest: &str, script: &str) -> std::path::PathBuf {
    let plugin = dir.path.join("plugins").join(name);
    std::fs::create_dir_all(&plugin).expect("a plugin dir");
    std::fs::write(plugin.join("manifest.toml"), manifest).expect("manifest.toml");
    std::fs::write(plugin.join("plugin.js"), script).expect("plugin.js");
    plugin
}

#[tokio::test]
async fn a_host_refusal_reaches_the_plugin_as_a_readable_sentence() {
    // Not decoration. A host error crosses into JavaScript as a thrown value,
    // and what a plugin's `catch (e) { return e.message }` reads is what the
    // *model* reads when that plugin reports bad news. An `rquickjs::Error`
    // carried across renders as "Error converting from js 'host' into type
    // 'value': ...", which is engine plumbing in front of the sentence that
    // matters, so a host refusal is thrown as a plain `Error` instead.
    let dir = TempDir::new("js-refusal");
    let kernel = kernel_in(&dir.path);
    let message = probe(
        &kernel,
        "reader",
        &[],
        "(() => { try { wizard.fs.read('/etc/hostname'); } catch (e) { return e.message; } })()",
    )
    .await;
    assert!(
        message.starts_with("sandboxed tool may not touch"),
        "the refusal has to read as itself: {message}"
    );
    assert!(
        !message.contains("converting"),
        "no engine plumbing in a message a model reads: {message}"
    );
}
