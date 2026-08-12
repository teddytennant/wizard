//! In-process LuaJIT runner for self-extension.
//!
//! Scripted tools ending in `.lua` (or whose manifest sets `runtime = "luajit"`)
//! execute here instead of spawning an external interpreter. Wizard embeds
//! LuaJIT — the just-in-time compiler — so evolve glue is fast, portable, and
//! does not depend on whatever shell/Python/Node happens to be on `PATH`.
//!
//! Contract for a Lua tool script:
//! - Tool arguments arrive as a Lua table in the global `args` (decoded from
//!   the JSON object the model passed).
//! - The project root is in the global `cwd` (string).
//! - Print results with `print(...)` (captured as the tool's stdout).
//! - Raise an error (or return a string starting with `"error:"`) to fail.
//! - Returning a non-nil value is treated as the result when nothing was
//!   printed; tables/values are JSON-encoded.
//!
//! # Two standard libraries, on purpose
//!
//! A script runs under one of two [`Stdlib`] profiles, and which one it gets
//! is decided by where the script came from, not by the script:
//!
//! - Locally authored tools (everything `/evolve` writes, everything the user
//!   drops in `~/.wizard/tools/`) run [`Stdlib::Full`], exactly as they always
//!   have. Their author is the user.
//! - Tools installed from the registry (`crate::registry_client`) run
//!   [`Stdlib::Sandboxed`] unless the user granted more at install time. Their
//!   author is a stranger.
//!
//! See [`Stdlib`] for what each profile actually opens, and why "safe" in
//! mlua's `StdLib::ALL_SAFE` never meant "cannot run commands".

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mlua::{Lua, LuaOptions, LuaSerdeExt, StdLib, Value as LuaValue};
use serde_json::Value as JsonValue;

use super::shell::{DEFAULT_TIMEOUT, MAX_TIMEOUT};
use super::{MAX_OUTPUT_BYTES, ToolError, ToolOutput, truncate_output};
use crate::registry_client::Trust;

/// Which Lua standard libraries a script gets, and the whole of Wizard's
/// answer to "a marketplace of installable tools is a supply chain".
///
/// [`Stdlib::Full`] is `StdLib::ALL_SAFE`, which is what every scripted tool
/// has run under since LuaJIT was embedded. It excludes `debug` and `ffi` but
/// keeps `os` and `io`, so `os.execute` is live: "safe" there means "cannot
/// corrupt the VM", never "cannot run commands". Locally authored tools keep
/// it, because their author is the user and evolve glue legitimately shells
/// out.
///
/// [`Stdlib::Sandboxed`] is what a registry-installed tool gets by default:
/// see [`sandboxed_libs`] for the exact set and [`BLANKED_GLOBALS`] for the
/// base-library functions that are blanked on top of it. A tool whose
/// published manifest declares capabilities can be granted [`Stdlib::Full`]
/// instead, but only through an explicit opt-in that names the author and what
/// is being granted (`crate::registry_client::decide_trust`).
///
/// No `Default` impl on purpose: at a trust boundary, the profile is a
/// decision every caller has to make out loud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stdlib {
    /// `StdLib::ALL_SAFE`: `os`, `io`, `package` and the rest. Not a sandbox.
    Full,
    /// Pure data processing: no `os`, no `io`, no `package`, and the host
    /// file helpers confined to the project directory.
    Sandboxed,
}

/// The libraries [`Stdlib::Sandboxed`] opens.
///
/// Deliberately an allowlist rather than `ALL_SAFE` minus the dangerous ones:
/// a blocklist silently widens the day mlua or LuaJIT grows a library, and
/// this is the one set whose accidental widening would be a supply-chain
/// hole. What is left out, and why:
///
/// - `os`: `os.execute` is a shell, `os.getenv` reads the API keys in this
///   process's environment, `os.remove`/`os.rename` are the filesystem.
/// - `io`: `io.open` is the filesystem and `io.popen` is a shell.
/// - `package`: `require` loads code off disk and `package.loadlib` loads a
///   native library, which is `ffi` by another name.
/// - `debug` and `ffi`: already outside `ALL_SAFE`, and they stay outside.
///
/// What is left in is pure: `table`, `string`, `math` and `bit` compute, and
/// `jit` only reports the runtime's own identity (it backs `wizard.version`).
/// Coroutines are part of the base library in LuaJIT and are not a flag.
///
/// The cost is real and is the accepted trade: a registry tool cannot read a
/// file outside the project, cannot shell out, and cannot even ask the clock.
fn sandboxed_libs() -> StdLib {
    StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::BIT | StdLib::JIT
}

/// Memory a sandboxed script may hold before the deadline hook stops it with
/// an ordinary Lua error. Generous for text munging, far below what it takes
/// to disturb the host.
const SANDBOX_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// How often the sandbox's deadline hook runs. Often enough that a tight loop
/// is caught promptly and that an allocating loop cannot get far past the
/// memory ceiling between two checks, rarely enough not to matter to an honest
/// script.
const SANDBOX_HOOK_INSTRUCTIONS: u32 = 10_000;

/// Base-library globals blanked for a [`Stdlib::Sandboxed`] run.
///
/// mlua opens `_G` unconditionally (it is not a `StdLib` flag), so leaving
/// `io` and `package` out does not by itself remove `dofile` and `loadfile`,
/// which open a path and execute it. `require`, `module`, `package`, `os`,
/// `io`, `debug` and `ffi` are absent already given [`sandboxed_libs`]; they
/// are blanked anyway so the guarantee holds even if that set is ever edited.
/// `load`/`loadstring` stay, but only for *text* — see [`blank_globals`], which
/// wraps them. The original reasoning here ("they compile a string against
/// these same globals, so they grant nothing the caller does not already
/// have") is true of source and false of bytecode: LuaJIT's loader defaults to
/// mode `"bt"` and has no bytecode verifier, so a binary chunk is the standard
/// LuaJIT sandbox escape — it reads and writes arbitrary memory and reaches
/// native code, which is every capability this allowlist exists to withhold.
///
/// `string.dump` goes with them: it turns any function a script can already
/// reach into exactly those bytes, which is where the chunk comes from.
const BLANKED_GLOBALS: [&str; 9] = [
    "dofile", "loadfile", "require", "module", "package", "os", "io", "debug", "ffi",
];

/// The profile a script at `script_path` runs under.
///
/// Registry installs leave a receipt next to the script recording what the
/// user granted (`crate::registry_client::trust_for_script`); anything with no
/// receipt is locally authored and keeps [`Stdlib::Full`], which is the
/// behaviour every existing tool was written against. An unreadable or
/// unparseable receipt resolves to [`Stdlib::Sandboxed`], so corrupting a
/// receipt file downgrades a tool instead of promoting one — see
/// [`crate::registry_client::trust_for_script`], which is where "unreadable"
/// has to be told apart from "absent" for that to be true.
pub fn resolve_stdlib(script_path: &Path) -> Stdlib {
    match crate::registry_client::trust_for_script(script_path) {
        Some(Trust::Full) => Stdlib::Full,
        Some(Trust::Sandboxed) => Stdlib::Sandboxed,
        None => Stdlib::Full,
    }
}

/// Run a LuaJIT script for a scripted tool, under the profile
/// [`resolve_stdlib`] derives from where the script lives.
///
/// `script` is the source; `script_path` is used in error messages *and* to
/// resolve the profile, so it must be the real on-disk path of the script.
/// `args` is the JSON object the model supplied; it becomes the global `args`.
pub fn run_scripted(
    tool: &str,
    script: &str,
    script_path: &Path,
    args: &JsonValue,
    cwd: &Path,
    timeout: Duration,
) -> Result<ToolOutput, ToolError> {
    run_scripted_with(
        tool,
        script,
        script_path,
        args,
        cwd,
        timeout,
        resolve_stdlib(script_path),
    )
}

/// [`run_scripted`] with the standard library chosen by the caller.
pub fn run_scripted_with(
    tool: &str,
    script: &str,
    script_path: &Path,
    args: &JsonValue,
    cwd: &Path,
    timeout: Duration,
    stdlib: Stdlib,
) -> Result<ToolOutput, ToolError> {
    let timeout = timeout.clamp(Duration::from_secs(1), MAX_TIMEOUT);
    let tool_name = tool.to_string();
    let script = script.to_string();
    let script_path = script_path.to_path_buf();
    let args = args.clone();
    let cwd = cwd.to_path_buf();

    // LuaJIT (and mlua's Lua handle under `send`) must not cross an await while
    // held on this thread's stack in ways that confuse the runtime; running the
    // whole chunk on a blocking pool worker keeps the agent loop free and lets
    // us enforce a wall-clock timeout with `spawn_blocking` + oneshot cancel is
    // awkward, so we use `tokio::time::timeout` around the join.
    let join = std::thread::Builder::new()
        .name(format!("wizard-luajit-{tool_name}"))
        .spawn(move || {
            run_lua_blocking(
                &tool_name,
                &script,
                &script_path,
                &args,
                &cwd,
                stdlib,
                timeout,
            )
        })
        .map_err(|err| ToolError::Execution {
            tool: tool.to_string(),
            source: anyhow::Error::new(err).context("failed to spawn LuaJIT worker"),
        })?;

    let start = std::time::Instant::now();
    loop {
        if join.is_finished() {
            return join.join().unwrap_or_else(|_| {
                Err(ToolError::Execution {
                    tool: tool.to_string(),
                    source: anyhow::anyhow!("LuaJIT worker panicked"),
                })
            });
        }
        if start.elapsed() >= timeout {
            // The worker cannot be safely aborted mid-Lua (no kill for a
            // foreign stack). Report timeout; the OS reaps the thread when the
            // chunk eventually returns. Tools should stay short.
            return Err(ToolError::Timeout {
                tool: tool.to_string(),
                seconds: timeout.as_secs().max(1),
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Async wrapper used by [`super::scripted::ScriptedTool`], under the profile
/// [`resolve_stdlib`] derives from where the script lives.
pub async fn run_scripted_async(
    tool: &str,
    script: &str,
    script_path: &Path,
    args: &JsonValue,
    cwd: &Path,
    timeout: Duration,
) -> Result<ToolOutput, ToolError> {
    let stdlib = resolve_stdlib(script_path);
    run_scripted_async_with(tool, script, script_path, args, cwd, timeout, stdlib).await
}

/// [`run_scripted_async`] with the standard library chosen by the caller.
pub async fn run_scripted_async_with(
    tool: &str,
    script: &str,
    script_path: &Path,
    args: &JsonValue,
    cwd: &Path,
    timeout: Duration,
    stdlib: Stdlib,
) -> Result<ToolOutput, ToolError> {
    let timeout = timeout.clamp(Duration::from_secs(1), MAX_TIMEOUT);
    let tool_name = tool.to_string();
    let script = script.to_string();
    let script_path = script_path.to_path_buf();
    let args = args.clone();
    let cwd = cwd.to_path_buf();

    match tokio::time::timeout(
        timeout,
        tokio::task::spawn_blocking(move || {
            run_lua_blocking(
                &tool_name,
                &script,
                &script_path,
                &args,
                &cwd,
                stdlib,
                timeout,
            )
        }),
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(join_err)) => Err(ToolError::Execution {
            tool: tool.to_string(),
            source: anyhow::Error::new(join_err).context("LuaJIT worker panicked"),
        }),
        Err(_) => Err(ToolError::Timeout {
            tool: tool.to_string(),
            seconds: timeout.as_secs().max(1),
        }),
    }
}

fn run_lua_blocking(
    tool: &str,
    script: &str,
    script_path: &Path,
    args: &JsonValue,
    cwd: &Path,
    stdlib: Stdlib,
    timeout: Duration,
) -> Result<ToolOutput, ToolError> {
    let libs = match stdlib {
        Stdlib::Full => StdLib::ALL_SAFE,
        Stdlib::Sandboxed => sandboxed_libs(),
    };
    let lua = Lua::new_with(libs, LuaOptions::default()).map_err(|err| ToolError::Execution {
        tool: tool.to_string(),
        source: anyhow::anyhow!("failed to create LuaJIT state: {err}"),
    })?;

    if stdlib == Stdlib::Sandboxed {
        // Turn the JIT off before the instruction hook below is installed, and
        // do it while `jit` is still reachable (`blank_globals` takes it away).
        //
        // LuaJIT compiles hot paths into traces, and a trace does not check for
        // a count hook the way the interpreter does: the hook the sandbox
        // installs stops being called once a loop is compiled, so the deadline
        // it enforces would silently stop applying to exactly the runaway loop
        // it exists to catch. Worse than not firing, erroring out of a hook
        // that fires *while a trace is running* unwinds through JIT-compiled
        // frames, which is what SIGSEGVs the suite on Apple Silicon.
        //
        // `jit.off(true, true)` disables compilation and flushes any traces
        // already recorded, so every sandboxed chunk runs interpreted, where
        // the hook is reliable. This costs a registry tool the JIT and buys the
        // bound being real; a locally authored tool (`Stdlib::Full`) is not
        // hooked at all and keeps the JIT.
        lua.load("jit.off(true, true)")
            .exec()
            .map_err(|err| ToolError::Execution {
                tool: tool.to_string(),
                source: anyhow::anyhow!("disabling the JIT for a sandboxed run: {err}"),
            })?;

        blank_globals(&lua).map_err(|err| ToolError::Execution {
            tool: tool.to_string(),
            source: anyhow::anyhow!("sandboxing the LuaJIT state: {err}"),
        })?;

        // A bound on how much a stranger's tool can take, not just on what it
        // can reach.
        //
        // The timeout above cannot stop a runaway chunk: there is no way to
        // abort a foreign Lua stack, so it reports and leaves the thread
        // running — `while true do end` burns a core for the life of the
        // process and holds one of tokio's blocking threads, the same pool
        // every native file tool uses. `t[#t+1] = string.rep("x", 1e6)` in a
        // loop OOM-kills the whole agent. SECURITY.md is honest that a
        // *locally authored* tool is bounded in time and not in capability; it
        // says nothing about a registry tool being able to wedge the process,
        // and a sandbox that stops `os.execute` but not `while true do end` is
        // only half a boundary.
        //
        // A memory ceiling turns the OOM into a Lua error the tool reports,
        // and an instruction hook turns the spin into one too. Both apply to
        // sandboxed runs only: a locally authored tool is the user's own code
        // and has always been allowed to take as long as it likes.
        // The ceiling is enforced by *watching* allocation, not by starving the
        // allocator.
        //
        // `set_memory_limit` makes the next allocation past the limit fail, and
        // handing LuaJIT an allocation failure is where this used to SIGSEGV on
        // Apple Silicon: LuaJIT on 64-bit does not properly support the foreign
        // allocator mlua installs to implement the limit, and its out-of-memory
        // path is the part that does not survive. It is a hard crash of the
        // whole process, so it takes the agent with it — strictly worse than
        // the OOM it was added to prevent.
        //
        // Reading `used_memory` from the hook below reaches the same ceiling
        // one step earlier, as an ordinary Lua error raised between
        // instructions rather than an allocation the VM cannot satisfy. The
        // hook runs often enough that a script cannot get far past the line
        // before it is stopped.
        let deadline = std::time::Instant::now() + timeout;
        lua.set_hook(
            mlua::HookTriggers::new().every_nth_instruction(SANDBOX_HOOK_INSTRUCTIONS),
            move |lua, _debug| {
                if std::time::Instant::now() >= deadline {
                    return Err(mlua::Error::runtime(
                        "sandboxed tool exceeded its time budget",
                    ));
                }
                // `used_memory` is 0 when mlua could not install its allocator,
                // which reads as "no reading available" rather than "nothing
                // allocated": the bound is skipped instead of tripping at once.
                let used = lua.used_memory();
                if used > SANDBOX_MEMORY_LIMIT {
                    return Err(mlua::Error::runtime(format!(
                        "sandboxed tool exceeded its memory budget ({} MB)",
                        SANDBOX_MEMORY_LIMIT / (1024 * 1024)
                    )));
                }
                Ok(mlua::VmState::Continue)
            },
        )
        .map_err(|err| ToolError::Execution {
            tool: tool.to_string(),
            source: anyhow::anyhow!("installing the sandbox deadline hook: {err}"),
        })?;
    }

    // Capture print() into a shared buffer the host reads after the chunk.
    let stdout: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    {
        let buf = Arc::clone(&stdout);
        let print_fn = lua
            .create_function(move |lua, values: mlua::MultiValue| {
                let mut line = String::new();
                for (i, value) in values.into_iter().enumerate() {
                    if i > 0 {
                        line.push('\t');
                    }
                    line.push_str(&lua_value_to_string(lua, value)?);
                }
                line.push('\n');
                if let Ok(mut guard) = buf.lock() {
                    guard.push_str(&line);
                }
                Ok(())
            })
            .map_err(|err| ToolError::Execution {
                tool: tool.to_string(),
                source: anyhow::anyhow!("installing print(): {err}"),
            })?;
        lua.globals()
            .set("print", print_fn)
            .map_err(|err| ToolError::Execution {
                tool: tool.to_string(),
                source: anyhow::anyhow!("setting print: {err}"),
            })?;
    }

    // args / cwd globals.
    let args_lua = json_to_lua(&lua, args).map_err(|err| ToolError::Execution {
        tool: tool.to_string(),
        source: anyhow::anyhow!("converting tool arguments to Lua: {err}"),
    })?;
    lua.globals()
        .set("args", args_lua)
        .map_err(|err| ToolError::Execution {
            tool: tool.to_string(),
            source: anyhow::anyhow!("setting args: {err}"),
        })?;
    lua.globals()
        .set("cwd", cwd_string(cwd))
        .map_err(|err| ToolError::Execution {
            tool: tool.to_string(),
            source: anyhow::anyhow!("setting cwd: {err}"),
        })?;

    // Lightweight std helpers so evolve glue does not need FFI for common work.
    install_wizard_lib(&lua, cwd, stdlib).map_err(|err| ToolError::Execution {
        tool: tool.to_string(),
        source: anyhow::anyhow!("installing wizard.* helpers: {err}"),
    })?;

    let chunk_name = format!("@{}", script_path.display());
    let result = lua
        .load(script)
        .set_name(&chunk_name)
        .eval::<LuaValue>()
        .map_err(|err| ToolError::Execution {
            tool: tool.to_string(),
            source: anyhow::anyhow!("LuaJIT error in {}:\n{err}", script_path.display()),
        })?;

    let printed = stdout.lock().map(|g| g.clone()).unwrap_or_default();

    let content = if !printed.is_empty() {
        printed
    } else {
        match result {
            LuaValue::Nil => String::new(),
            other => lua_value_to_json_string(&lua, other).unwrap_or_else(|err| err.to_string()),
        }
    };

    // Convention: scripts may signal soft failure by returning/printing a
    // line that starts with "error:".
    let trimmed = content.trim_start();
    let is_error = trimmed.starts_with("error:") || trimmed.starts_with("Error:");
    let content = truncate_output(content, MAX_OUTPUT_BYTES);
    if is_error {
        Ok(ToolOutput::error(content))
    } else {
        Ok(ToolOutput::ok(content))
    }
}

fn cwd_string(cwd: &Path) -> String {
    cwd.to_string_lossy().into_owned()
}

/// Blank every [`BLANKED_GLOBALS`] name. Setting an already-absent global to
/// `nil` is a no-op, so this is safe to run over any library set.
fn blank_globals(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    for name in BLANKED_GLOBALS {
        globals.set(name, LuaValue::Nil)?;
    }

    // Text chunks only. A `load` that accepts bytecode is not a compiler, it is
    // a way out of the sandbox: LuaJIT will not verify a binary chunk, so a
    // crafted one reaches arbitrary memory and native code. Both spellings are
    // wrapped, and `string.dump` — which is where a script would get a valid
    // chunk from — is removed, because a script that can dump a function it
    // already holds can feed the result straight back in.
    //
    // The wrapper mirrors Lua's own contract: it returns `nil, message` on a
    // refusal rather than raising, so an honest caller's error handling is
    // unchanged.
    let load_text =
        lua.create_function(|lua, (chunk, name): (mlua::LuaString, Option<String>)| {
            let bytes = chunk.as_bytes();
            if bytes.first() == Some(&0x1b) {
                return Ok((
                    LuaValue::Nil,
                    Some("bytecode chunks are not allowed".to_string()),
                ));
            }
            let name = name.unwrap_or_else(|| "=(load)".to_string());
            match lua.load(bytes.as_ref()).set_name(name).into_function() {
                Ok(function) => Ok((LuaValue::Function(function), None)),
                Err(err) => Ok((LuaValue::Nil, Some(err.to_string()))),
            }
        })?;
    globals.set("load", load_text.clone())?;
    globals.set("loadstring", load_text)?;
    if let Ok(string) = globals.get::<mlua::Table>("string") {
        string.set("dump", LuaValue::Nil)?;
    }
    Ok(())
}

/// `wizard.read_file`, `wizard.write_file`, `wizard.json_encode/decode` —
/// small host bridge so Lua tools do real work without shelling out.
///
/// Under [`Stdlib::Sandboxed`] the two file helpers are confined to the
/// project directory (see [`resolve_tool_path`]). Removing `io` while leaving
/// `wizard.write_file` pointed at `~/.ssh/authorized_keys` would be exactly
/// the "shipping neither answer and calling it safe" this profile exists to
/// avoid.
fn install_wizard_lib(lua: &Lua, cwd: &Path, stdlib: Stdlib) -> mlua::Result<()> {
    let table = lua.create_table()?;
    let cwd_read = PathBuf::from(cwd);
    let cwd_write = PathBuf::from(cwd);

    let read_file = lua.create_function(move |_, path: String| {
        let p = resolve_tool_path(&cwd_read, &path, stdlib)?;
        std::fs::read_to_string(&p).map_err(mlua::Error::external)
    })?;
    table.set("read_file", read_file)?;

    let write_file = lua.create_function(move |_, (path, contents): (String, String)| {
        let p = resolve_tool_path(&cwd_write, &path, stdlib)?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(mlua::Error::external)?;
        }
        std::fs::write(&p, contents).map_err(mlua::Error::external)
    })?;
    table.set("write_file", write_file)?;

    let json_encode = lua.create_function(|lua, value: LuaValue| {
        let json = lua_to_json(lua, value).map_err(mlua::Error::external)?;
        serde_json::to_string(&json).map_err(mlua::Error::external)
    })?;
    table.set("json_encode", json_encode)?;

    let json_decode = lua.create_function(|lua, raw: String| {
        let json: JsonValue = serde_json::from_str(&raw).map_err(mlua::Error::external)?;
        json_to_lua(lua, &json).map_err(mlua::Error::external)
    })?;
    table.set("json_decode", json_decode)?;

    // Identity marker so scripts (and doctors) can see they are on LuaJIT.
    table.set("runtime", "luajit")?;
    table.set(
        "version",
        lua.load("return jit and jit.version or _VERSION")
            .eval::<String>()
            .unwrap_or_else(|_| "Lua".into()),
    )?;

    lua.globals().set("wizard", table)?;
    Ok(())
}

/// Where `wizard.read_file`/`write_file` may point, per profile.
///
/// [`Stdlib::Full`] keeps the original behaviour: `~` expands and an absolute
/// path is taken as written, because the tool's author is the user.
///
/// [`Stdlib::Sandboxed`] treats the project directory as the whole world. An
/// absolute path or a leading `~` is refused rather than quietly re-rooted (a
/// tool that asked for `/etc/passwd` should fail loudly, not silently read
/// `<cwd>/etc/passwd`), `..` is resolved lexically and refused when it climbs
/// out, and an existing path is canonicalized and re-checked so a symlink
/// planted inside the project cannot point out of it.
fn resolve_tool_path(cwd: &Path, path: &str, stdlib: Stdlib) -> mlua::Result<PathBuf> {
    if stdlib == Stdlib::Full {
        return Ok(resolve_against(cwd, path));
    }
    confine_to(cwd, path).map_err(|reason| {
        mlua::Error::external(anyhow::anyhow!(
            "sandboxed tool may not touch '{path}': {reason}. \
             Registry tools are confined to the project directory; \
             a tool that needs more has to declare it and be installed with an explicit grant."
        ))
    })
}

fn resolve_against(cwd: &Path, path: &str) -> PathBuf {
    let expanded = shellexpand::tilde(path);
    let candidate = Path::new(expanded.as_ref());
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

/// Resolve `path` strictly inside `root`, or say why it cannot be.
/// Purely lexical up to the symlink check, so it is unit-testable without
/// building the tree.
fn confine_to(root: &Path, path: &str) -> Result<PathBuf, String> {
    if path.starts_with('~') {
        return Err("home-relative paths are outside the project".to_string());
    }
    let mut rel = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => rel.push(part),
            Component::ParentDir => {
                if !rel.pop() {
                    return Err("'..' climbs out of the project directory".to_string());
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("absolute paths are outside the project".to_string());
            }
        }
    }
    let joined = root.join(&rel);

    // The lexical result is inside `root` by construction; the remaining hole
    // is a symlink. Canonicalize the deepest ancestor that exists (the target
    // itself for a read, its parent chain for a write into a new file) and
    // require it to still sit under the canonical root.
    let root_real = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut probe = joined.as_path();
    let resolved = loop {
        match probe.canonicalize() {
            Ok(real) => break real,
            Err(_) => match probe.parent() {
                Some(parent) => probe = parent,
                None => return Err("no part of the path exists".to_string()),
            },
        }
    };
    if !resolved.starts_with(&root_real) {
        return Err("resolves outside the project directory (symlink?)".to_string());
    }

    // The hole the loop above leaves: a *dangling* symlink. It exists, so a
    // write through it creates its target — but it does not canonicalize, so
    // the probe walked straight past it to a parent that does, and the lexical
    // path was handed back. `project/x -> ../../.wizard/hooks.toml` with no
    // target yet is a checked-in symlink away from arbitrary code at the next
    // session start, from inside the profile whose whole purpose is
    // confinement.
    //
    // Every component the probe skipped is by definition one that failed to
    // canonicalize. If any of them is a symlink, we cannot say where the write
    // lands, so we refuse rather than guess.
    let mut walk = joined.as_path();
    while walk != probe {
        if std::fs::symlink_metadata(walk).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err("path goes through an unresolvable symlink".to_string());
        }
        match walk.parent() {
            Some(parent) => walk = parent,
            None => break,
        }
    }
    Ok(joined)
}

fn json_to_lua(lua: &Lua, value: &JsonValue) -> mlua::Result<LuaValue> {
    // mlua's serde feature: Value implements Serialize/Deserialize via
    // Lua ser/de — use from_value on the JSON side through serde_json -> Lua.
    lua.to_value(value)
}

fn lua_to_json(lua: &Lua, value: LuaValue) -> mlua::Result<JsonValue> {
    lua.from_value(value)
}

fn lua_value_to_string(lua: &Lua, value: LuaValue) -> mlua::Result<String> {
    match value {
        LuaValue::Nil => Ok("nil".into()),
        LuaValue::Boolean(b) => Ok(b.to_string()),
        LuaValue::Integer(i) => Ok(i.to_string()),
        LuaValue::Number(n) => Ok(n.to_string()),
        LuaValue::String(s) => Ok(s.to_str()?.to_owned()),
        other => {
            // tostring() via Lua for tables/userdata.
            let tostring: mlua::Function = lua.globals().get("tostring")?;
            tostring.call::<String>(other)
        }
    }
}

fn lua_value_to_json_string(lua: &Lua, value: LuaValue) -> mlua::Result<String> {
    match &value {
        LuaValue::String(s) => Ok(s.to_str()?.to_owned()),
        LuaValue::Nil => Ok(String::new()),
        _ => {
            let json = lua_to_json(lua, value)?;
            Ok(serde_json::to_string_pretty(&json).unwrap_or_default())
        }
    }
}

/// True when a scripted tool should run through the embedded LuaJIT runtime.
pub fn is_luajit_tool(
    script_path: &Path,
    interpreter: Option<&str>,
    runtime: Option<&str>,
) -> bool {
    if runtime.is_some_and(|r| {
        let r = r.trim().to_ascii_lowercase();
        r == "luajit" || r == "lua" || r == "embedded"
    }) {
        return true;
    }
    if let Some(interp) = interpreter {
        let i = interp.to_ascii_lowercase();
        if i.contains("luajit") || i == "lua" || i.ends_with("/lua") || i.ends_with("/luajit") {
            return true;
        }
    }
    script_path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("lua"))
}

/// Default timeout helper re-export for callers that do not want shell deps.
#[allow(dead_code)]
pub fn default_timeout() -> Duration {
    DEFAULT_TIMEOUT
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Temp project root removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("wizard-lua-{label}-{}", uuid::Uuid::new_v4()));
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

    /// Run `script` against `cwd` under an explicit profile.
    fn run(script: &str, cwd: &Path, stdlib: Stdlib) -> Result<ToolOutput, ToolError> {
        run_scripted_with(
            "t",
            script,
            &cwd.join("t.lua"),
            &json!({}),
            cwd,
            Duration::from_secs(10),
            stdlib,
        )
    }

    fn error_text(err: &ToolError) -> String {
        match err {
            ToolError::Execution { source, .. } => format!("{err:#}\n{source:#}"),
            other => format!("{other:#}"),
        }
    }

    #[test]
    fn detects_lua_by_extension_and_runtime() {
        assert!(is_luajit_tool(Path::new("x.lua"), None, None));
        assert!(is_luajit_tool(Path::new("x.sh"), None, Some("luajit")));
        assert!(is_luajit_tool(Path::new("x.sh"), Some("luajit"), None));
        assert!(!is_luajit_tool(Path::new("x.sh"), Some("bash"), None));
    }

    #[test]
    fn runs_print_and_args() {
        let out = run_scripted(
            "t",
            r#"print("hello", args.name)"#,
            Path::new("t.lua"),
            &json!({"name": "wizard"}),
            Path::new("."),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("hello"), "{}", out.content);
        assert!(out.content.contains("wizard"), "{}", out.content);
    }

    #[test]
    fn return_value_used_when_silent() {
        let out = run_scripted(
            "t",
            r#"return args.n * 2"#,
            Path::new("t.lua"),
            &json!({"n": 21}),
            Path::new("."),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content.trim() == "42" || out.content.contains("42"),
            "{}",
            out.content
        );
    }

    #[test]
    fn lua_error_becomes_tool_error() {
        let err = run_scripted(
            "t",
            r#"error("boom")"#,
            Path::new("t.lua"),
            &json!({}),
            Path::new("."),
            Duration::from_secs(5),
        )
        .unwrap_err();
        let full = match &err {
            ToolError::Execution { source, .. } => format!("{err:#}\n{source:#}"),
            other => format!("{other:#}"),
        };
        assert!(
            full.contains("boom") || full.contains("LuaJIT error"),
            "{full}"
        );
    }

    #[test]
    fn wizard_json_roundtrip() {
        let out = run_scripted(
            "t",
            r#"
local enc = wizard.json_encode(args)
local dec = wizard.json_decode(enc)
print(dec.x)
print(wizard.runtime)
"#,
            Path::new("t.lua"),
            &json!({"x": "ok"}),
            Path::new("."),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(out.content.contains("ok"), "{}", out.content);
        assert!(out.content.contains("luajit"), "{}", out.content);
    }

    #[test]
    fn soft_error_prefix() {
        let out = run_scripted(
            "t",
            r#"print("error: nope")"#,
            Path::new("t.lua"),
            &json!({}),
            Path::new("."),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn async_wrapper_works() {
        let out = run_scripted_async(
            "t",
            r#"return "async-ok""#,
            Path::new("t.lua"),
            &json!({}),
            Path::new("."),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(out.content.contains("async-ok"), "{}", out.content);
    }

    // -- the sandboxed profile ---------------------------------------------

    #[test]
    fn the_sandboxed_library_set_is_an_allowlist_and_stays_one() {
        // The runtime blanks `os`/`io`/`package` as well, which means a test
        // driving a script cannot tell whether a library was never opened or
        // was opened and then hidden. This asserts the primary guarantee
        // directly, so widening the set is caught even though the second layer
        // would mask it.
        let libs = sandboxed_libs();
        for (name, lib) in [
            ("os", StdLib::OS),
            ("io", StdLib::IO),
            ("package", StdLib::PACKAGE),
            ("debug", StdLib::DEBUG),
            ("ffi", StdLib::FFI),
        ] {
            assert!(
                !libs.contains(lib),
                "the sandboxed profile opened `{name}`; registry tools would get it"
            );
        }
        for (name, lib) in [
            ("table", StdLib::TABLE),
            ("string", StdLib::STRING),
            ("math", StdLib::MATH),
        ] {
            assert!(libs.contains(lib), "a sandboxed tool needs `{name}`");
        }

        // And the reason it is an allowlist: `ALL_SAFE` is every bit below the
        // two it excludes, so a new mlua library is inside it the day it is
        // added. Deriving the sandbox by subtraction would widen silently.
        assert!(StdLib::ALL_SAFE.contains(StdLib::OS));
        assert!(StdLib::ALL_SAFE.contains(StdLib::IO));
    }

    #[test]
    fn a_sandboxed_script_has_no_os_no_io_and_no_file_loaders() {
        let tmp = TempDir::new("libs");
        let out = run(
            r#"
local names = {"os", "io", "package", "require", "dofile", "loadfile", "debug", "ffi", "module"}
local reachable = {}
for _, name in ipairs(names) do
  if _G[name] ~= nil then table.insert(reachable, name) end
end
return "reachable:[" .. table.concat(reachable, ",") .. "]"
"#,
            &tmp.0,
            Stdlib::Sandboxed,
        )
        .expect("the script itself runs");
        assert!(
            out.content.contains("reachable:[]"),
            "a registry tool reached a library it must not have: {}",
            out.content
        );

        // The pure libraries a data-processing tool actually needs are there,
        // so "sandboxed" still means "useful".
        let out = run(
            r#"return table.concat({type(table), type(string), type(math), type(bit)}, ",")"#,
            &tmp.0,
            Stdlib::Sandboxed,
        )
        .expect("runs");
        assert!(
            out.content.contains("table,table,table,table"),
            "{}",
            out.content
        );
    }

    /// The sandbox refuses precompiled bytecode.
    ///
    /// `load`/`loadstring` were left in on the reasoning that they "compile a
    /// string against these same globals, so they grant nothing the caller does
    /// not already have". That holds for *text*. LuaJIT's loader defaults to
    /// mode `"bt"` and has no bytecode verifier, so a binary chunk is the
    /// canonical LuaJIT sandbox escape — crafted bytecode reads and writes
    /// arbitrary memory and reaches native code, which is every capability the
    /// allowlist exists to withhold.
    ///
    /// This is the registry supply chain: a published tool passes checksum
    /// verification, installs as `Sandboxed`, and escapes on first call.
    #[test]
    fn a_sandboxed_script_cannot_load_bytecode() {
        let tmp = TempDir::new("bytecode");

        // Valid bytecode, produced the way an attacker would: `string.dump`
        // turns a function the script already has into a binary chunk and
        // `load` reads it back. A malformed chunk proves nothing — LuaJIT
        // rejects it at parse — so the test has to actually reach the bytecode
        // loader.
        for global in ["load", "loadstring"] {
            let script = format!(
                "local dump = string.dump\n\
                 if not dump then return 'refused: no string.dump' end\n\
                 local chunk = dump(function() return 7 end)\n\
                 local f, err = {global}(chunk)\n\
                 if f then return 'ACCEPTED' end\n\
                 return 'refused: ' .. tostring(err)\n"
            );
            let out = run(&script, &tmp.0, Stdlib::Sandboxed)
                .unwrap_or_else(|err| panic!("{global} script ran: {}", error_text(&err)));
            assert!(
                !out.content.contains("ACCEPTED"),
                "{global} accepted a binary chunk — the sandbox is escapable: {}",
                out.content
            );
        }

        // `string.dump` is the other half: it turns any function the script can
        // reach into exactly the bytes above.
        let out =
            run("return tostring(string.dump)", &tmp.0, Stdlib::Sandboxed).expect("script runs");
        assert!(
            out.content.contains("nil"),
            "string.dump is reachable, which hands a script a bytecode factory: {}",
            out.content
        );
    }

    /// The wrapper still compiles ordinary source, and still reports errors the
    /// way Lua does. Closing the bytecode hole must not cost a sandboxed tool
    /// its ability to build a function at run time.
    /// A symlink whose target does not exist yet cannot be written through.
    ///
    /// `confine_to` canonicalizes the deepest ancestor that exists, and a
    /// dangling symlink is not one — so the probe walked past it to the project
    /// root, which passes, and handed back the lexical path. `fs::write`
    /// follows symlinks, so the write landed outside.
    ///
    /// The reachable version: a repository ships `payload ->
    /// ../../.wizard/hooks.toml` (git stores whatever target you like), a
    /// sandboxed registry tool writes to `payload`, and the *global* hooks file
    /// — which SECURITY.md documents as ungated — runs whatever it wants at the
    /// next session start.
    #[test]
    fn a_dangling_symlink_cannot_be_written_through() {
        let tmp = TempDir::new("dangling");
        let outside = tmp.0.parent().expect("a parent").join("escaped.txt");
        let _ = std::fs::remove_file(&outside);

        std::os::unix::fs::symlink("../escaped.txt", tmp.0.join("payload"))
            .expect("create the dangling symlink");

        let err = run(
            "wizard.write_file('payload', 'PWNED')\nreturn 'wrote'",
            &tmp.0,
            Stdlib::Sandboxed,
        )
        .expect_err("a write through a dangling symlink must be refused");
        let text = error_text(&err);
        assert!(
            text.contains("symlink"),
            "expected a symlink refusal, got: {text}"
        );
        assert!(
            !outside.exists(),
            "the sandboxed script wrote outside the project: {}",
            outside.display()
        );
    }

    #[test]
    fn a_sandboxed_script_can_still_load_text() {
        let tmp = TempDir::new("loadtext");

        let out = run(
            "local f = load('return 6 * 7')\nreturn tostring(f())",
            &tmp.0,
            Stdlib::Sandboxed,
        )
        .expect("text chunks still compile");
        assert!(out.content.contains("42"), "{}", out.content);

        // A syntax error is `nil, message`, not a raise.
        let out = run(
            "local f, err = load('this is not lua')\n\
             if f then return 'compiled' end\n\
             return 'err: ' .. tostring(err)",
            &tmp.0,
            Stdlib::Sandboxed,
        )
        .expect("a bad chunk must not raise");
        assert!(out.content.starts_with("err: "), "{}", out.content);
    }

    /// A sandboxed script cannot spin or allocate the agent to death.
    ///
    /// The timeout above cannot stop a runaway chunk — there is no way to abort
    /// a foreign Lua stack, so it reports and leaves the thread running. Before
    /// these bounds, `while true do end` in a registry tool burned a core for
    /// the life of the process and held one of tokio's blocking threads (the
    /// pool every native file tool shares), and a loop appending megabyte
    /// strings OOM-killed the whole agent.
    ///
    /// A sandbox that stops `os.execute` but not `while true do end` is half a
    /// boundary. Both bounds are sandboxed-only: a locally authored tool is the
    /// user's own code and has always been allowed to take as long as it likes.
    #[test]
    fn a_sandboxed_script_is_bounded_in_time_and_memory() {
        let tmp = TempDir::new("bounds");

        // A tight loop ends as a reported error, not a hung process.
        let start = std::time::Instant::now();
        let err = run_scripted_with(
            "spin",
            "while true do end",
            &tmp.0.join("spin.lua"),
            &json!({}),
            &tmp.0,
            Duration::from_millis(700),
            Stdlib::Sandboxed,
        )
        .expect_err("an infinite loop must not run forever");
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "the spin was not interrupted: {:?}",
            start.elapsed()
        );
        let text = error_text(&err);
        assert!(
            text.contains("time budget") || matches!(err, ToolError::Timeout { .. }),
            "expected a time-budget refusal, got: {text}"
        );

        // Runaway allocation becomes a Lua error the tool reports.
        let err = run_scripted_with(
            "greedy",
            "local t = {}\nwhile true do t[#t + 1] = string.rep('x', 1000000) end",
            &tmp.0.join("greedy.lua"),
            &json!({}),
            &tmp.0,
            Duration::from_secs(20),
            Stdlib::Sandboxed,
        )
        .expect_err("unbounded allocation must not reach the host allocator");
        let text = error_text(&err);
        assert!(
            text.contains("memory") || text.contains("time budget"),
            "expected a memory refusal, got: {text}"
        );

        // And an ordinary script is untouched by either bound.
        let out = run(
            "local s = 0\nfor i = 1, 200000 do s = s + i end\nreturn tostring(s)",
            &tmp.0,
            Stdlib::Sandboxed,
        )
        .expect("an honest script still runs");
        assert!(out.content.contains("20000100000"), "{}", out.content);
    }

    #[test]
    fn a_sandboxed_script_cannot_run_a_command() {
        let tmp = TempDir::new("exec");
        let marker = tmp.path("pwned.txt");
        let script = format!(
            "os.execute(\"touch '{}'\")\nreturn 'ran'\n",
            marker.display()
        );

        let err = run(&script, &tmp.0, Stdlib::Sandboxed)
            .expect_err("os.execute must not be reachable from a registry tool");
        let text = error_text(&err);
        assert!(
            text.contains("nil"),
            "expected an indexing error on a nil `os`, got: {text}"
        );
        assert!(
            !marker.exists(),
            "the sandboxed script ran a shell command: {} exists",
            marker.display()
        );
    }

    #[test]
    fn the_full_profile_is_not_a_sandbox_and_never_claimed_to_be() {
        // This is the honest half of the answer written down as a test: mlua's
        // ALL_SAFE keeps `os`, so a full-stdlib tool runs commands. Locally
        // authored tools have always had this and keep it; a registry tool
        // only gets here through an explicit grant.
        let tmp = TempDir::new("full");
        let marker = tmp.path("ran.txt");
        let script = format!(
            "os.execute(\"touch '{}'\")\nreturn type(os.execute) .. '/' .. type(io.open)\n",
            marker.display()
        );
        let out = run(&script, &tmp.0, Stdlib::Full).expect("the full profile runs it");
        assert!(out.content.contains("function/function"), "{}", out.content);
        assert!(
            marker.exists(),
            "os.execute is live under the full profile; if this failed, the profiles were swapped"
        );
    }

    #[test]
    fn sandboxed_file_helpers_stay_inside_the_project() {
        let tmp = TempDir::new("confine");
        std::fs::write(tmp.path("inside.txt"), "in\n").unwrap();
        let outside = tmp.path("outside-secret.txt");
        std::fs::create_dir_all(tmp.path("project")).unwrap();
        std::fs::write(&outside, "secret\n").unwrap();
        let project = tmp.path("project");
        std::fs::write(project.join("ok.txt"), "fine\n").unwrap();

        // Inside the project: unchanged behaviour.
        let out = run(
            r#"return wizard.read_file("ok.txt")"#,
            &project,
            Stdlib::Sandboxed,
        )
        .expect("reads inside the project");
        assert!(out.content.contains("fine"), "{}", out.content);

        // Out of it: refused, whichever way it is spelled.
        for path in ["../outside-secret.txt", "/etc/passwd", "~/.ssh/id_rsa"] {
            let script = format!("return wizard.read_file(\"{path}\")");
            let err = match run(&script, &project, Stdlib::Sandboxed) {
                Ok(out) => panic!("{path} must be refused, but the tool read: {}", out.content),
                Err(err) => err,
            };
            let text = error_text(&err);
            assert!(
                text.contains("sandboxed tool may not touch"),
                "{path}: {text}"
            );
        }

        // Writes are confined too, and nothing lands outside.
        let escape = tmp.path("escaped.txt");
        let script = r#"wizard.write_file("../escaped.txt", "x")"#;
        let err = run(script, &project, Stdlib::Sandboxed).expect_err("write must be refused");
        assert!(
            error_text(&err).contains("climbs out"),
            "{}",
            error_text(&err)
        );
        assert!(!escape.exists());

        // The full profile is unchanged: absolute paths still work.
        let script = format!("return wizard.read_file(\"{}\")", outside.display());
        let out = run(&script, &project, Stdlib::Full).expect("full profile reads anywhere");
        assert!(out.content.contains("secret"), "{}", out.content);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_project_does_not_widen_the_sandbox() {
        let tmp = TempDir::new("symlink");
        let project = tmp.path("project");
        std::fs::create_dir_all(&project).unwrap();
        let secret = tmp.path("secret.txt");
        std::fs::write(&secret, "secret\n").unwrap();
        std::os::unix::fs::symlink(&secret, project.join("link.txt")).unwrap();

        let err = run(
            r#"return wizard.read_file("link.txt")"#,
            &project,
            Stdlib::Sandboxed,
        )
        .expect_err("a symlink out of the project is still out of the project");
        assert!(error_text(&err).contains("symlink"), "{}", error_text(&err));
    }

    #[test]
    fn confine_to_normalizes_before_it_decides() {
        let tmp = TempDir::new("lexical");
        let root = &tmp.0;
        std::fs::create_dir_all(root.join("a")).unwrap();

        // `a/../b.txt` stays inside and is allowed even though it holds `..`.
        assert_eq!(
            confine_to(root, "a/../b.txt"),
            Ok(root.join("b.txt")),
            "a `..` that does not escape is fine"
        );
        assert_eq!(confine_to(root, "./a/x.txt"), Ok(root.join("a/x.txt")));
        assert!(confine_to(root, "../x").is_err());
        assert!(confine_to(root, "a/../../x").is_err());
        assert!(confine_to(root, "/etc/passwd").is_err());
        assert!(confine_to(root, "~/x").is_err());
    }

    #[test]
    fn the_profile_comes_from_the_install_receipt_not_the_script() {
        let tmp = TempDir::new("profile");
        let script = tmp.path("mine.lua");
        std::fs::write(&script, "return 1\n").unwrap();

        // No receipt: locally authored, and locally authored tools keep the
        // behaviour every existing tool was written against.
        assert_eq!(resolve_stdlib(&script), Stdlib::Full);

        // A registry install without a grant is sandboxed.
        let receipt = json!({
            "name": "mine",
            "kind": "tool",
            "author": "alice",
            "version": "1.0.0",
            "checksum": "0".repeat(64),
            "source": "https://example.invalid/tools/alice/mine/tool.lua",
            "installed_at": "2026-01-01T00:00:00Z",
            "trust": "sandboxed",
        });
        let receipt_path = crate::registry_client::receipt_for_script(&script);
        std::fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert_eq!(resolve_stdlib(&script), Stdlib::Sandboxed);

        // With one, it is full.
        let granted = json!({
            "name": "mine",
            "kind": "tool",
            "author": "alice",
            "version": "1.0.0",
            "checksum": "0".repeat(64),
            "source": "https://example.invalid/tools/alice/mine/tool.lua",
            "installed_at": "2026-01-01T00:00:00Z",
            "trust": "full",
            "capabilities": ["process"],
        });
        std::fs::write(&receipt_path, serde_json::to_vec(&granted).unwrap()).unwrap();
        assert_eq!(resolve_stdlib(&script), Stdlib::Full);
    }

    #[tokio::test]
    async fn the_async_path_sandboxes_a_registry_tool_too() {
        // `ScriptedTool::execute` goes through `run_scripted_async`, which is
        // where a real installed tool arrives. The receipt has to be honoured
        // there, not just in the sync entry point.
        let tmp = TempDir::new("asyncsandbox");
        let script = tmp.path("theirs.lua");
        std::fs::write(&script, "return 1\n").unwrap();
        let receipt = json!({
            "name": "theirs",
            "kind": "tool",
            "author": "alice",
            "version": "1.0.0",
            "checksum": "0".repeat(64),
            "source": "https://example.invalid/tools/alice/theirs/tool.lua",
            "installed_at": "2026-01-01T00:00:00Z",
            "trust": "sandboxed",
        });
        std::fs::write(
            crate::registry_client::receipt_for_script(&script),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();

        let out = run_scripted_async(
            "theirs",
            r#"return type(os) .. "/" .. type(io)"#,
            &script,
            &json!({}),
            &tmp.0,
            Duration::from_secs(10),
        )
        .await
        .expect("runs");
        assert!(out.content.contains("nil/nil"), "{}", out.content);
    }
}
