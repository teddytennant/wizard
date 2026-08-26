-- `publish`: fork Wizard to the user's GitHub and hand back a one-line
-- installer for their variant.
--
-- This was two files. `src/tools/publish.rs` was a twenty-line adapter that
-- parsed one argument and formatted one string, and `src/evolve/publish.rs`
-- was the body: nine steps, every one of them an argv, an exit code and a
-- message. `docs/plugins.md` named that body "the most Lua-shaped in the tree"
-- before the bridge existed to move it, and this is that move. Porting only
-- the adapter would have produced a host call whose body was the Rust — the
-- "Rust with a slower calling convention" the same document opens by refusing.
--
-- Two things it must not become. It must not be a second GitHub client: `gh`
-- holds the credentials, knows what a fork is and prints its own diagnostics,
-- so every API question here is a `gh` invocation and the only parsing is one
-- `.login` out of one JSON object. And it must not be a second opinion about
-- where Wizard keeps its state: `wizard.paths` carries `Config`'s own answers,
-- which is what keeps this plugin inside the temp directory `cargo test`
-- redirects `~/.wizard` to instead of writing into a developer's real home.

--- The repository forks are made from. A fork is of *upstream*, not of
--- whatever the local checkout's `origin` happens to be, so this is a constant
--- rather than a `git remote get-url origin`: a user who already forked once
--- has an `origin` pointing at their own fork, and forking that would make a
--- fork of a fork under a name nobody expects.
local UPSTREAM_SLUG = "teddytennant/wizard"

--- Where the checkout is cloned from when there is not one yet. The same
--- environment override deep evolve reads, because it is the same checkout.
local DEFAULT_REPO_URL = "https://github.com/teddytennant/wizard"

--- Budget for a `gh` or `git` call that only asks a question. Generous for a
--- request that is one round trip, short enough that an unauthenticated `gh`
--- sitting on a prompt is a failure rather than a hang.
local QUICK_MS = 60 * 1000

--- Budget for `gh repo fork`, which creates a repository on the far side and
--- is the one API call that is not instant.
local FORK_MS = 2 * 60 * 1000

--- Budget for the two calls that move a repository over the network. The first
--- push to a fresh fork is the whole history, and this is the number the old
--- Rust had no equivalent of at all — it used a blocking `Command::output()`
--- with no timeout, on the async runtime, so a stalled push wedged the turn
--- with nothing to interrupt it.
local TRANSFER_MS = 10 * 60 * 1000

--- Rust's `str::trim`, which is what every `String::from_utf8_lossy(...).trim()`
--- in the old code applied before looking at a captured stream.
local function trim(text)
  return (text:gsub("^%s+", ""):gsub("%s+$", ""))
end

--- Run one program and hand back the whole outcome.
---
--- `argv` rather than a shell line, for `git_status`'s reason and one more of
--- this plugin's own: a branch name comes from the model and a fork slug comes
--- from GitHub, and a shell line would mean quoting both correctly, in Lua,
--- forever.
local function run(argv, timeout_ms)
  return wizard.process.exec { argv = argv, timeout_ms = timeout_ms }
end

--- `git -C <dir> <args...>`.
local function git(dir, args, timeout_ms)
  local argv = { "git", "-C", dir }
  for i = 1, #args do
    argv[#argv + 1] = args[i]
  end
  return run(argv, timeout_ms or QUICK_MS)
end

--- Whatever a failed command said, preferring stderr and falling back to
--- stdout.
---
--- Both, because the two tools disagree about which one they use: `git`
--- diagnoses on stderr and `gh` sometimes answers on stdout even when it
--- exits non-zero. A renderer that read only stderr would report some `gh`
--- failures as a bare exit code, which is the one thing the model cannot act
--- on.
local function said(result)
  local stderr = trim(result.stderr)
  if stderr ~= "" then
    return stderr
  end
  return trim(result.stdout)
end

--- Fail the tool with `message`, as an error rather than as an exception.
---
--- `error()` would mean the plugin *broke*. Every failure below is a plugin
--- that worked and has bad news — `gh` is not installed, the push was
--- rejected — and the difference is what the model reads: a broken tool is
--- worth retrying, a rejected push is worth reading.
local function fail(message)
  return {
    content = wizard.truncate("publish failed: " .. message, wizard.limits.error),
    is_error = true,
  }
end

--- `true` when `<path>` can be opened for reading.
---
--- The check the old `ensure_source` spelled `dir.join("Cargo.toml").is_file()`.
--- A directory opens successfully on Linux and fails on the first read, which
--- does not matter here: every path this is asked about is a regular file.
local function readable(path)
  local handle = io.open(path, "r")
  if handle == nil then
    return false
  end
  handle:close()
  return true
end

--- Make sure `~/.wizard/src` holds a Wizard checkout, cloning one on first
--- use, and return its path.
---
--- The same checkout deep evolve builds in, which is the point: `/publish`
--- after a deep evolve pushes the source that produced the running binary.
---
--- One behaviour is deliberately git's rather than ours. The Rust version
--- read the directory first so it could say "exists but does not look like a
--- Wizard checkout" itself; Lua has no `read_dir`, and inventing a host call
--- for one predicate would be a wider bridge to say something `git clone`
--- already says ("destination path ... already exists and is not an empty
--- directory"). So the clone is attempted and git's own sentence is what
--- comes back, with the remedy appended.
local function ensure_source()
  local dir = wizard.paths.source
  if dir == nil then
    return nil, "could not locate ~/.wizard (no home directory?)"
  end
  if readable(dir .. "/Cargo.toml") then
    return dir
  end

  local url = os.getenv("WIZARD_SOURCE_REPO")
  if url == nil or trim(url) == "" then
    url = DEFAULT_REPO_URL
  end
  -- `git clone` creates the leading directories itself, so `~/.wizard` need
  -- not exist yet.
  local cloned = run({ "git", "clone", "--depth", "1", url, dir }, TRANSFER_MS)
  if cloned.code ~= 0 then
    local detail = said(cloned)
    if detail:find("already exists", 1, true) then
      detail = detail .. "\nRemove " .. dir .. " and retry."
    end
    return nil, "cloning " .. url .. " into " .. dir .. " failed: " .. detail
  end
  if not readable(dir .. "/Cargo.toml") then
    return nil, "cloned " .. url .. " but no Cargo.toml is in " .. dir
  end
  return dir
end

--- `"<owner>/wizard"` — the slug a fork of upstream lands under. The repo name
--- is always upstream's, because that is what GitHub names a fork.
local function fork_slug(owner)
  return owner .. "/wizard"
end

--- The one-liner anybody can run to install this fork.
---
--- Kept to the character, including the two spaces of nothing where the
--- Rust's line continuation was: `install.sh` reads `WIZARD_REPO`,
--- `WIZARD_REF` and `WIZARD_BUILD_FROM_SOURCE`, and a one-liner that drifts
--- from those three names is a one-liner that installs stock Wizard while
--- claiming to install somebody's fork.
local function install_one_liner(owner, repo, ref)
  return string.format(
    "curl -fsSL https://raw.githubusercontent.com/%s/%s/%s/install.sh | "
      .. "WIZARD_REPO=%s/%s WIZARD_REF=%s WIZARD_BUILD_FROM_SOURCE=1 bash",
    owner,
    repo,
    ref,
    owner,
    repo,
    ref
  )
end

--- Pull `.login` out of `gh api user`.
local function parse_gh_login(raw)
  local ok, decoded = pcall(wizard.json_decode, raw)
  if not ok or type(decoded) ~= "table" then
    return nil, "`gh api user` did not answer with JSON"
  end
  if type(decoded.login) ~= "string" or decoded.login == "" then
    return nil, "`.login` is not in the `gh api user` response"
  end
  return decoded.login
end

--- Append one publish record to `~/.wizard/evolution.jsonl`.
---
--- Best-effort, and a failure is not the user's problem: the fork exists and
--- the one-liner is in their hands whether or not a line reached a log. The
--- `event` key is the discriminator `read_events` in `src/evolve/mod.rs`
--- already skips on, so this line is a record with no reader today and must
--- stay one that does not break the reader it has.
local function log_publish(record)
  local path = wizard.paths.evolution_log
  if path == nil then
    return
  end
  local ok, line = pcall(wizard.json_encode, record)
  if not ok then
    return
  end
  local handle = io.open(path, "a")
  if handle == nil then
    return
  end
  -- One write for the record and its newline. Two writes under `O_APPEND` is
  -- how two processes logging at the same moment interleave into a line
  -- neither of them wrote, which is what `append_line` in `src/evolve/mod.rs`
  -- exists to explain.
  handle:write(line .. "\n")
  handle:close()
end

--- The whole of `/publish`, in the order the steps have to happen.
---
--- Returns a tool result table either way: this function's failures are all
--- the tool reporting bad news, never the tool breaking.
local function do_publish(branch)
  local source_dir, err = ensure_source()
  if source_dir == nil then
    return fail(err)
  end

  -- `gh` present, then `gh` authenticated. Two checks and not one, because
  -- the remedies are different sentences and a user who has neither should be
  -- told to install before being told to log in.
  if run({ "gh", "--version" }, QUICK_MS).code ~= 0 then
    return fail(
      "`gh` (the GitHub CLI) is required to publish; "
        .. "install it from https://cli.github.com and run `gh auth login`"
    )
  end
  if run({ "gh", "auth", "status" }, QUICK_MS).code ~= 0 then
    return fail(
      "not authenticated with GitHub — run `gh auth login` first, "
        .. "then retry `wizard --publish`"
    )
  end

  local who = run({ "gh", "api", "user" }, QUICK_MS)
  if who.code ~= 0 then
    return fail("`gh api user` failed: " .. said(who))
  end
  local login, login_err = parse_gh_login(who.stdout)
  if login == nil then
    return fail(login_err)
  end

  local fork_repo = fork_slug(login)
  local fork_url = "https://github.com/" .. fork_repo

  -- Forking is idempotent from the user's point of view and not from `gh`'s:
  -- it exits non-zero when the fork is already there, and that is the common
  -- case for anyone publishing a second time.
  local forked = run({ "gh", "repo", "fork", UPSTREAM_SLUG, "--clone=false" }, FORK_MS)
  if forked.code ~= 0 and not said(forked):find("already exists", 1, true) then
    return fail("`gh repo fork " .. UPSTREAM_SLUG .. "` failed: " .. said(forked))
  end

  -- Whether it was just created or was already there, the fork has to be
  -- reachable under this account before anything is pushed at it. This is the
  -- check that catches an expired token and a fork somebody deleted.
  if run({ "gh", "repo", "view", fork_repo }, QUICK_MS).code ~= 0 then
    return fail(
      "fork `"
        .. fork_repo
        .. "` could not be accessed after forking; run `gh repo view "
        .. fork_repo
        .. "` to diagnose"
    )
  end

  -- Read before the push, so the sha reported is the one that was sent.
  local head = git(source_dir, { "rev-parse", "--short", "HEAD" })
  local commit = nil
  if head.code == 0 and trim(head.stdout) ~= "" then
    commit = trim(head.stdout)
  end

  -- `set-url` rather than `add` when the remote is there, because the owner
  -- can change: somebody who deletes a fork and re-forks under an
  -- organisation leaves a `fork` remote pointing at a repository that is gone.
  local remote_url = fork_url .. ".git"
  local exists = git(source_dir, { "remote", "get-url", "fork" }).code == 0
  local verb = exists and "set-url" or "add"
  local wired = git(source_dir, { "remote", verb, "fork", remote_url })
  if wired.code ~= 0 then
    return fail("`git remote " .. verb .. " fork " .. remote_url .. "` failed: " .. said(wired))
  end

  local pushed = git(source_dir, { "push", "fork", "HEAD:" .. branch }, TRANSFER_MS)
  if pushed.code ~= 0 then
    return fail("`git push fork HEAD:" .. branch .. "` failed: " .. said(pushed))
  end

  local one_liner = install_one_liner(login, "wizard", branch)
  log_publish {
    event = "publish",
    timestamp = os.date("!%Y-%m-%dT%H:%M:%SZ"),
    fork_repo = fork_repo,
    fork_url = fork_url,
    branch = branch,
    install_one_liner = one_liner,
    commit = commit,
  }

  local sha = commit and ("  commit: " .. commit) or ""
  return {
    content = wizard.truncate(
      string.format(
        "Published to %s  (branch: %s)%s\n\nInstall one-liner:\n%s",
        fork_url,
        branch,
        sha,
        one_liner
      ),
      wizard.limits.output
    ),
  }
end

return {
  name = "publish",

  apply = function(ctx)
    ctx:tool {
      name = "publish",
      description = "Fork Wizard to your own GitHub account and get a one-line installer "
        .. "for your personalised variant. Use this after a deep evolve (or any "
        .. "time you want to distribute the version of Wizard running on this "
        .. "machine). The fork is created under your authenticated GitHub account "
        .. "via `gh`; the source checkout at ~/.wizard/src is pushed to the fork "
        .. "and a `curl | bash` one-liner is returned that anyone can run to "
        .. "install your variant (building from source). Requires `gh auth login`.",
      -- No `required` key, where the native tool wrote `"required": []`.
      -- The two mean the same thing to every tool-calling API, and the
      -- spelling matters because Lua has one table type: `required = {}` is a
      -- table with no entries and comes back as the JSON object `{}`, which is
      -- not an empty array and is not a valid schema. `object_schema` repairs
      -- that at `properties`, where an object is what was wanted; there is no
      -- repair for the places an empty *array* was wanted, and the honest fix
      -- for a key whose empty value carries no information is to leave it out.
      parameters = {
        type = "object",
        properties = {
          branch = {
            type = "string",
            description = 'Branch to push to on the fork. Defaults to "main".',
          },
        },
      },
      -- Not `read_only`: this one writes to somebody's GitHub account. The
      -- native tool declared nothing and got `Execute` by default, and the
      -- default is what it wanted — plan mode must not publish.
      execute = function(args)
        args = args or {}
        local branch = args.branch
        if branch ~= nil and type(branch) ~= "string" then
          error("publish: 'branch' must be a string", 0)
        end
        if branch == nil or trim(branch) == "" then
          branch = "main"
        end
        return do_publish(branch)
      end,
    }
  end,
}
