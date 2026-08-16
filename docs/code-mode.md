# Code mode (`run_code`)

Code mode gives the model one extra tool: `run_code`, which runs a LuaJIT
program that can call Wizard's own tools.

It is off by default.

```toml
# ~/.wizard/config.toml
code_mode = true
```

`WIZARD_CODE_MODE=1` turns it on for one process, `WIZARD_CODE_MODE=0` turns it
off. Anything else leaves the config alone.

## What it is for

Round trips. A model that wants to read forty files and keep the three that
match makes forty tool calls, and the other thirty-seven land in its context on
the way past. In code mode it writes one loop and reads three lines.

```lua
local hits = {}
for _, path in ipairs({"a.rs", "b.rs", "c.rs"}) do
  local text = tool.read_file{path = path}
  if text:find("TODO") then hits[#hits + 1] = path end
end
print(table.concat(hits, "\n"))
```

The rule the tool's own description gives the model: use it when three or more
calls would otherwise be a fixed sequence, or when the next call's arguments are
computable from the previous call's output. Do not use it when you need to read
a result before deciding what to do next, because then the round trip is the
point.

## The Lua a program gets

The interpreter is the one already in the binary — the same LuaJIT that runs
scripted tools under `~/.wizard/tools/`. There is nothing to install.

- `tool.<name>{...}` dispatches a Wizard tool. `tool.read_file{path="x"}`.
  The name can also be spelled `tool["playwright__browser_click"]{ref="e7"}`,
  which is how MCP tools are reached.
- `wizard.call(name, args)` is the same thing as a status table
  (`{ok=, content=, status=}`) that never raises.
- `wizard.tools()` lists what is callable.
- `print(...)` is what comes back. Everything else is thrown away.
- `args` (an empty table), `cwd`, `wizard.read_file`, `wizard.write_file`,
  `wizard.json_encode`, `wizard.json_decode`, `wizard.runtime` and
  `wizard.version` are the same globals a scripted tool has.

`wizard.read_file` and `tool.read_file` are not the same call. The `wizard.*`
pair are raw filesystem helpers that no hook sees and no checkpoint covers,
which is what they have meant in every scripted tool ever written. The
dispatched read is `tool.read_file`. The namespace is the distinction.

### Three answers, and Lua tells them apart

| The tool | In Lua |
|---|---|
| ran and succeeded | returns the result string |
| ran and reported failure | returns `nil, message` |
| could not be run at all | raises |

The middle row is the one that matters. A failing build is diagnostic signal,
not a malfunction, so it comes back as a value the program can read and act on.
The bottom row means nothing happened on the machine, so nothing downstream
would be computing on real data; wrap it in `pcall` if you meant to probe.

## Nothing survives a call

One interpreter per `run_code` call, dropped when the call returns. Globals,
functions and loaded data are gone. A second call starts from nothing.

This is a decision, not a limitation waiting to be lifted. A persistent
interpreter would be a Lua heap that `/rewind` cannot restore, `/resume` cannot
replay, `/fork` cannot copy, and compaction cannot summarise — so the model
would believe things were defined that were not, at five separate points, in the
one feature Wizard sells as reversible.

State that has to outlive a program goes in a file, through `wizard.write_file`
or the `memory` tool. A file survives compaction, resume, fork, rewind and a
restart.

## What comes back

Success:

```
run_code ok (3 tool calls, 0.42s compute)

output:
parsed 1841 rows
3 files over the threshold

calls:
  1 read_file {"path":"Cargo.toml"} -> ok, 3841 bytes
  2 search_files {"pattern":"mlua"} -> ok, 812 bytes
  3 edit_file {"path":"src/x.rs"} -> ok
```

Failure, every kind, the same shape: a header naming the kind, the message,
whatever the program printed before it failed, and the ledger of calls that
already ran. The ledger is the difference between the model retrying safely and
writing the same file twice.

There are seven ways to fail, and they are kept apart on purpose because the
right reaction differs:

| Header | What happened | What to do about it |
|---|---|---|
| `run_code compile:` | the program would not parse, so nothing in it ran | fix the Lua |
| `run_code error:` | the program raised | read the traceback and the ledger |
| `run_code denied:` | a tool call inside it was refused | stop trying; the Lua is fine |
| `run_code time:` | it used its compute budget | narrow the work |
| `run_code memory:` | it held more than 64 MB | stream instead of accumulating |
| `run_code calls:` | more than 64 dispatched calls | narrow the loop |
| `run_code interrupted:` | the user stopped the turn | nothing |

Only `compile` is treated as a fault by the circuit breakers, because it is the
only one where the call could not be made at all. `error` is a program's `exit
1` and is bounded by the ordinary per-tool backstop instead.

Printing more than the output cap is **not** a failure. It truncates and spills
the rest to a file, exactly like any other tool's output.

## Bounds

| Bound | Value |
|---|---|
| compute | 30 s by default, 120 s maximum, set per call with `timeout_secs` |
| wall clock | 600 s, never extended |
| memory | 64 MB of Lua heap, checked between instructions |
| printed output | 8 MB held, then further output is dropped |
| dispatched calls | 64 |

The compute budget does not count time spent inside a tool call. A program that
runs a two-minute build has not spent two minutes of its own budget, because the
work was the build's.

The wall clock is the backstop that makes that safe: it is never extended, so a
program whose every call is slow cannot push its deadline forward forever.

The bounds are read between VM instructions, and a bound that fires stops the
program for good: catching it with `pcall` does not let it carry on, and a
coroutine it created is bounded like the main chunk. Two things follow from
"between instructions", and both are real limits rather than fine print:

- **One allocation can pass the memory ceiling.** `string.rep('x', 6e8)` is a
  single instruction, so the check has no chance to run in the middle of it. The
  alternative — handing LuaJIT a failing allocator — crashes the process on some
  platforms, so it is not used. Build big strings in pieces if you build them
  at all.
- **Nothing fires inside a C call.** A program parked in `os.execute("sleep
  99999")` cannot be stopped from inside. The turn is not held hostage by it —
  the host stops waiting a couple of seconds after the budget and reports what
  the program printed — but the thread runs until its call returns. The
  supported way to run a command from a program is `tool.execute`, which has a
  timeout of its own.

## Hooks, checkpoints and plan mode

Every tool a program calls goes through the same dispatch pipeline a direct call
does:

- `pre_tool_use` hooks can rewrite its arguments or veto it. A veto arrives as
  `run_code denied:`. This covers `tool.<name>{...}` and `wizard.call`, which is
  every *dispatched* call — it is not a filesystem policy: `io.open` and
  `wizard.write_file` are raw calls that no hook sees and no checkpoint covers,
  the same as in any scripted tool. `tool.write_file` is the dispatched write.
- `Edit`-class calls are snapshotted under the parent's current turn, so
  `/rewind` undoes a program's edits.
- `post_tool_use` hooks run and their output is appended.

Plan mode refuses `run_code` outright rather than letting a program start and
hit a wall halfway: the tool is `Execute`-class, so the plan gate blocks it with
the message that names `exit_plan`.

A program cannot call `run_code`, `spawn_subagent`, `evolve`, `publish`,
`exit_plan`, `interview` or `run_command`. Programs do not nest and cannot
delegate.

## Why it is off by default, and what it does not claim

A program is code the model wrote, running in-process, with your privileges and
the full Lua standard library. `os` and `io` are live. That is the same standing
a scripted tool you wrote yourself has, and the same standing the `execute` tool
already gives the model — a program that can call `tool.execute` is not made
safer by taking `os.execute` away from it.

So: what is bounded is time, memory and call count. Capability is not bounded,
with one exception — `os.exit` is removed, because there is no `tool.exit` and it
is the only call a program has that ends the host process rather than itself.
And the bound does not hold inside a C call, as above.

See [SECURITY.md](../SECURITY.md).

It is also never offered to a model without native tool calling, whatever the
config says. Those models get their tool roster through a prompt-based JSON
protocol, and a multi-line Lua program inside a JSON string, emitted by hand by
a model that already struggles with two-field calls, does not fail loudly — it
stalls the turn.
