-- `git_status` and `git_diff`: the working tree, as the model sees it.
--
-- This was `src/tools/git.rs` and is the first native tool set to leave Rust.
-- It was picked because it is the shape `docs/plugins.md` calls Lua-shaped and
-- almost nothing else in `src/tools/` is: every line of it decides an argv,
-- reads an exit code, and formats a string. There is no protocol to frame, no
-- shared type core also holds, and no field of `ToolContext` it needs beyond
-- the directory to run in.
--
-- What it must not become is a second implementation of anything. The capture
-- buffer, the process group, the timeout kill and the cancel handle are
-- `wizard.process.exec`; the head/tail framing and the spill file are
-- `wizard.truncate`; the byte budgets are `wizard.limits`. What is written
-- here is only the part that was ever git-specific.

--- How long git gets. Status and diff are local, so anything slower than this
--- is a wedged repository rather than a big one, and the host clamps it to the
--- `[shell]` foreground budget anyway.
local GIT_TIMEOUT_MS = 30 * 1000

--- Rust's `str::trim_end`, which is what the native tool applied to every
--- captured stream before looking at it. Parenthesised because `gsub` returns
--- the count as a second value and `return` would carry it along.
local function trim_end(text)
  return (text:gsub("%s+$", ""))
end

--- `str::lines().count()` for a string with no trailing newline.
local function line_count(text)
  if text == "" then
    return 0
  end
  local lines = 1
  for _ in text:gmatch("\n") do
    lines = lines + 1
  end
  return lines
end

--- Run `git <args...>` in `cwd`.
---
--- `argv` and not a command line. The path in `git_diff` comes from the model,
--- and a shell line would mean quoting it correctly here, in Lua, forever.
local function run_git(cwd, args)
  local argv = { "git" }
  for i = 1, #args do
    argv[#argv + 1] = args[i]
  end
  return wizard.process.exec {
    argv = argv,
    cwd = cwd,
    timeout_ms = GIT_TIMEOUT_MS,
  }
end

--- The shell tool's rendering of a command that outlived its budget: whatever
--- it had produced, then the reason it stopped.
---
--- Kept identical to `render_command_result`'s timeout arm rather than
--- summarised, because this string is the one the model reads to decide
--- whether to retry, and "timed out ... output above is partial" is what tells
--- it the answer above is not the whole answer.
local function timed_out_output(result)
  local stdout = trim_end(result.stdout)
  local stderr = trim_end(result.stderr)

  local content = ""
  if stdout ~= "" then
    content = stdout
  end
  if stderr ~= "" then
    if content ~= "" then
      content = content .. "\n"
    end
    content = content .. "stderr:\n" .. stderr
  end

  local note
  if content == "" then
    note = string.format(
      "command timed out after %ds and was killed (no output produced)",
      result.timed_out
    )
  else
    content = content .. "\n"
    note = string.format(
      "command timed out after %ds and was killed; output above is partial",
      result.timed_out
    )
  end

  return {
    content = wizard.truncate(content .. note, wizard.limits.output),
    is_error = true,
  }
end

--- Model-facing output for a git invocation that did not exit 0.
---
--- git says why on stderr and says it well, so the fallback is only for the
--- case where it exited non-zero and said nothing — which happens, and leaves
--- the model with a bare failure it cannot act on otherwise. Capped at the
--- error budget rather than the output one: stderr this long has stopped being
--- a message.
local function git_failure(result, fallback)
  if result.timed_out then
    return timed_out_output(result)
  end
  local stderr = trim_end(result.stderr)
  local detail = stderr ~= "" and stderr or fallback
  return {
    content = wizard.truncate(detail, wizard.limits.error),
    is_error = true,
  }
end

--- Reject an argument whose type the schema already ruled out.
---
--- The native tool got this from `serde`, which refused the call before it ran.
--- A Lua tool is handed the decoded JSON with no schema behind it, so a
--- `staged = "yes"` would otherwise be truthy and quietly diff the index.
local function expect(args, field, wanted, tool)
  local value = args[field]
  if value ~= nil and type(value) ~= wanted then
    error(string.format("%s: '%s' must be a %s", tool, field, wanted), 0)
  end
  return value
end

return {
  name = "git",

  apply = function(ctx)
    ctx:tool {
      name = "git_status",
      description = "Show the git working tree status of the project (branch, staged, modified, and untracked files).",
      -- No `parameters`: the host's own empty schema is the object with no
      -- properties every tool-calling API wants, and writing `properties = {}`
      -- here would be a Lua table with no entries, which serialises as `[]`.
      access = "read_only",
      execute = function(_args, tool)
        local result = run_git(tool.cwd, { "status", "--porcelain=v1", "-b" })
        if result.code ~= 0 then
          return git_failure(result, "git status failed")
        end

        local status = trim_end(result.stdout)
        -- Porcelain v1 with `-b` always emits a `## branch` header first, so a
        -- header-only output means a clean tree.
        local content = status
        if line_count(status) <= 1 then
          content = status .. "\n(clean working tree)"
        end
        return { content = wizard.truncate(content, wizard.limits.listing) }
      end,
    }

    ctx:tool {
      name = "git_diff",
      description = "Show the git diff of the project: unstaged changes by default, staged with staged=true, optionally limited to one path.",
      parameters = {
        type = "object",
        properties = {
          staged = {
            type = "boolean",
            description = "Diff staged changes instead of the working tree",
          },
          path = {
            type = "string",
            description = "Limit the diff to this path",
          },
        },
      },
      access = "read_only",
      execute = function(args, tool)
        args = args or {}
        local staged = expect(args, "staged", "boolean", "git_diff")
        local path = expect(args, "path", "string", "git_diff")

        local argv = { "diff" }
        if staged then
          argv[#argv + 1] = "--cached"
        end
        if path ~= nil then
          argv[#argv + 1] = "--"
          argv[#argv + 1] = path
        end

        local result = run_git(tool.cwd, argv)
        if result.code ~= 0 then
          return git_failure(result, "git diff failed")
        end

        local diff = trim_end(result.stdout)
        if diff == "" then
          return "No changes."
        end
        -- A diff gets a diff's budget and not an arbitrary command's: 16 KB is
        -- about 400 lines, past which the useful move is a narrower path.
        return { content = wizard.truncate(diff, wizard.limits.diff) }
      end,
    }
  end,
}
