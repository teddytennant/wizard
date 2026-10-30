//! Running a command line through the platform shell.
//!
//! Wizard hands whole command *lines* to a shell rather than an argv vector,
//! everywhere: the `execute` tool, background tasks, hooks, MCP stdio servers,
//! scripted tools, and the TUI's external editor (`"code --wait"` has to work
//! as a setting, so it cannot be `Command::new(editor)`). On Unix that means
//! `sh -c`, which was written out by hand in five files
//! (`tools/shell.rs`, `tools/tasks.rs`, `app/term.rs`, `evolve/mod.rs`,
//! `hooks/mod.rs`) and is now written here once.
//!
//! Windows has no `sh`. `cmd.exe /C` and `powershell -Command` take different
//! flags, quote differently, and are different languages; picking one is a
//! decision that belongs here rather than in every caller. [`name`] exists so
//! the *model* learns the answer too: the system prompt has to say which shell
//! is active, because a model that assumes `sh` will write `ls | grep` where
//! PowerShell needs `Get-ChildItem`.

/// A [`std::process::Command`] that runs `command_line` through the platform
/// shell. The caller still owns the rest of the configuration (working
/// directory, stdio, environment).
pub fn command(command_line: &str) -> std::process::Command {
    let (program, flag) = invocation();
    let mut command = std::process::Command::new(program);
    command.arg(flag).arg(command_line);
    command.envs(session_env().iter().map(|(k, v)| (k, v)));
    command
}

/// Variables every command line inherits on top of the process environment:
/// the `env` map from Claude-format `settings.json` files. Set once per
/// agent build; empty unless a settings file asks for some.
static SESSION_ENV: std::sync::RwLock<Vec<(String, String)>> = std::sync::RwLock::new(Vec::new());

/// Replace the session environment (see [`SESSION_ENV`]).
pub fn set_session_env(vars: Vec<(String, String)>) {
    if let Ok(mut current) = SESSION_ENV.write() {
        *current = vars;
    }
}

fn session_env() -> Vec<(String, String)> {
    SESSION_ENV.read().map(|v| v.clone()).unwrap_or_default()
}

/// [`command`], for the async half of the codebase. Same shell, same flag: the
/// two must never drift, so they read the same [`invocation`].
pub fn tokio_command(command_line: &str) -> tokio::process::Command {
    let (program, flag) = invocation();
    let mut command = tokio::process::Command::new(program);
    command.arg(flag).arg(command_line);
    command.envs(session_env().iter().map(|(k, v)| (k, v)));
    command
}

/// [`tokio_command`], with a pipeline's failure reported as the command's own
/// failure.
///
/// A pipeline's exit status is its *last* command's, so
/// `apt-get install -y python3 | tail -20` exits 0 when apt fails. The
/// `execute` tool ran that way, the model was told the install had worked, and
/// it built on a package that was never there. `pipefail` makes the status the
/// last failing stage's instead.
///
/// Which shell provides it is [`pipefail`]'s problem. When nothing here does,
/// the line runs exactly as it used to: a wrong exit code beats no command.
pub fn tokio_command_pipefail(command_line: &str) -> tokio::process::Command {
    match pipefail() {
        // Same shell, same language, one option set first. The prelude is on
        // the same line so a syntax error still reports the line the model
        // wrote as line 1.
        Pipefail::Prelude => tokio_command(&format!("set -o pipefail; {command_line}")),
        Pipefail::Bash => {
            let mut command = tokio::process::Command::new("bash");
            command
                .arg("-o")
                .arg("pipefail")
                .arg("-c")
                .arg(command_line);
            command.envs(session_env().iter().map(|(k, v)| (k, v)));
            command
        }
        Pipefail::Unsupported => tokio_command(command_line),
    }
}

/// Whether [`tokio_command_pipefail`] actually gets `pipefail` here. The
/// `execute` tool asks so it only explains a pipeline's exit code when the
/// explanation is true.
pub fn pipefail_supported() -> bool {
    pipefail() != Pipefail::Unsupported
}

/// How `pipefail` is reachable on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pipefail {
    /// The platform shell takes `set -o pipefail`.
    Prelude,
    /// It does not (dash is the common case), but `bash` is installed.
    Bash,
    /// Neither. Windows `cmd` has no such concept either.
    Unsupported,
}

/// [`Pipefail`] for this host, probed once.
///
/// Probed rather than assumed because `set -o pipefail` is not POSIX and dash
/// treats it as a fatal error in a non-interactive shell: guessing wrong kills
/// every command instead of one. The probe costs two process spawns at most,
/// on the first command the agent runs, not at startup.
fn pipefail() -> Pipefail {
    static RESOLVED: std::sync::OnceLock<Pipefail> = std::sync::OnceLock::new();
    *RESOLVED.get_or_init(|| {
        #[cfg(unix)]
        {
            if probe(command("set -o pipefail")) {
                return Pipefail::Prelude;
            }
            let mut bash = std::process::Command::new("bash");
            bash.arg("-o").arg("pipefail").arg("-c").arg("exit 0");
            if probe(bash) {
                return Pipefail::Bash;
            }
        }
        Pipefail::Unsupported
    })
}

/// Run `probe` with its stdio discarded and report whether it exited 0.
#[cfg(unix)]
fn probe(mut probe: std::process::Command) -> bool {
    probe
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The active shell's name, as a user (or a model) would say it.
///
/// Deliberately the shell Wizard *runs commands with*, not `$SHELL`: the
/// system prompt uses this to tell the model what syntax its `execute` calls
/// will be interpreted as, and the user's login shell is not that. On Unix
/// they are usually both "sh"-ish anyway; on Windows they are not related at
/// all.
pub fn name() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| display_name(program()))
}

/// The program [`command`] spawns: `sh` from `PATH` unless something said
/// otherwise; see [`resolve_program`].
pub fn program() -> &'static str {
    invocation().0
}

/// Pin the shell to `program`: `[shell] program` in `config.toml`, which
/// [`crate::config::Config::load`] passes here. The first call wins, and only
/// if it comes before the first command runs, so one process never runs two
/// shells or tells the model about one it is not using.
pub fn set_program(program: &str) {
    let _ = CONFIGURED.set(program.to_string());
}

static CONFIGURED: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Which shell runs command lines, decided once.
///
/// `[shell] program` wins everywhere. Past that, desktop Unix keeps `sh` from
/// `PATH` and deliberately ignores `$SHELL` (see [`name`]). Android is the
/// exception on both counts: an app has no `/bin`, and the shell an app ships
/// sits in its native library directory as `libbash.so`, a name no `PATH`
/// lookup finds. The app says where it is through `$SHELL`, and
/// `/system/bin/sh`, which every Android has, covers an app that does not.
#[cfg(unix)]
fn resolve_program() -> String {
    if let Some(program) = CONFIGURED.get() {
        return program.clone();
    }
    #[cfg(target_os = "android")]
    {
        if let Some(shell) = std::env::var_os("SHELL")
            .map(std::path::PathBuf::from)
            .filter(|shell| shell.is_absolute() && shell.is_file())
        {
            return shell.to_string_lossy().into_owned();
        }
        "/system/bin/sh".to_string()
    }
    #[cfg(not(target_os = "android"))]
    {
        "sh".to_string()
    }
}

/// What a shell calls itself: the file name, with the `lib…so` wrapping an
/// Android APK forces on bundled executables taken off (`libbash.so` is
/// `bash`).
fn display_name(program: &str) -> String {
    let file = std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    file.strip_prefix("lib")
        .and_then(|rest| rest.strip_suffix(".so"))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(file)
        .to_string()
}

/// A shebang line that runs a script under the platform shell.
///
/// Test-only, and public because three modules' tests write executable shell
/// scripts (the fake `cargo` the deep-evolve kill probe drives, the running
/// binary [`super::exe_swap`] replaces underneath itself). They all used to
/// hard-code `#!/bin/sh`, which is the one spelling [`invocation`] documents as
/// wrong: Termux has no `/bin` at all, and its `sh` lives at `$PREFIX/bin/sh`.
/// A shebang cannot ask `PATH` the way `Command::new("sh")` does, so ask the
/// shell where it lives and write the absolute answer.
#[cfg(test)]
pub fn shebang() -> String {
    // `command -v` is POSIX and built into every candidate shell, so this
    // resolves the same `sh` that `command()` would spawn rather than a guess.
    let resolved = command(&format!("command -v {}", program()))
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|path| std::path::Path::new(path).is_absolute());
    match resolved {
        Some(path) => format!("#!{path}"),
        // Nothing on `PATH` resolved: the FHS spelling is the only remaining
        // guess, and a script that cannot start says so loudly enough.
        None => format!("#!/bin/{}", name()),
    }
}

/// The shell binary and the flag that means "run this string".
///
/// One function so [`command`], [`tokio_command`] and [`name`] cannot disagree
/// about what the active shell is. `sh` unqualified rather than `/bin/sh`:
/// Termux has no `/bin`, and its `sh` lives at `$PREFIX/bin/sh` on `PATH`.
/// [`resolve_program`] says what can replace it.
fn invocation() -> (&'static str, &'static str) {
    #[cfg(unix)]
    {
        static PROGRAM: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        (PROGRAM.get_or_init(resolve_program).as_str(), "-c")
    }
    // The Windows arm is a choice, not a translation: `cmd /C` is present on
    // every machine but is a poor language, while PowerShell (`-NoProfile
    // -Command`) is what a Windows user writes today. Whichever wins, `name`
    // has to change with it or the system prompt starts lying.
    #[cfg(not(unix))]
    {
        ("cmd", "/C")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The program and arguments a built command would exec, as strings.
    fn parts(command: &std::process::Command) -> (String, Vec<String>) {
        (
            command.get_program().to_string_lossy().into_owned(),
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
        )
    }

    #[test]
    fn a_command_line_goes_to_the_shell_as_one_argument() {
        // The whole point of the shell surface: pipes, quotes and redirection
        // survive because the line is passed as a single argument, not split.
        let line = "echo 'a b' | wc -l > /dev/null";
        let (spawned, args) = parts(&command(line));
        assert_eq!(spawned, program());
        assert_eq!(args.len(), 2, "expected <flag> <line>, got {args:?}");
        assert_eq!(args[1], line);
    }

    #[test]
    fn the_pipefail_line_still_reaches_the_shell_whole() {
        // Whichever arm resolves, the model's line is one argument and the
        // last one: a prelude may be prepended to it, nothing may split it.
        let line = "grep 'a|b' f | sort -u > out";
        let command = tokio_command_pipefail(line);
        let (_, args) = parts(command.as_std());
        assert!(
            args.last().is_some_and(|last| last.ends_with(line)),
            "{args:?}"
        );
    }

    #[tokio::test]
    async fn a_failing_stage_makes_the_pipeline_fail() {
        assert!(
            pipefail_supported(),
            "no shell here takes `set -o pipefail` and there is no bash to fall back to"
        );
        let failed = tokio_command_pipefail("exit 9 | cat")
            .status()
            .await
            .expect("the shell ran");
        assert_eq!(failed.code(), Some(9), "the stage's code, not cat's 0");
        let worked = tokio_command_pipefail("echo hi | cat")
            .stdout(std::process::Stdio::null())
            .status()
            .await
            .expect("the shell ran");
        assert!(worked.success(), "a working pipeline is untouched");
    }

    #[test]
    fn the_async_and_blocking_surfaces_agree() {
        // Two spawn paths, one shell. If these ever diverge, a hook and the
        // same command in the `execute` tool would run under different rules.
        let line = "true";
        let (blocking_program, blocking_args) = parts(&command(line));
        let async_command = tokio_command(line);
        let async_command = async_command.as_std();
        let (async_program, async_args) = parts(async_command);
        assert_eq!(blocking_program, async_program);
        assert_eq!(blocking_args, async_args);
    }

    #[test]
    fn the_shell_named_here_is_the_one_that_actually_runs() {
        // `name()` feeds the system prompt, so it has to be the shell the tool
        // calls really land in, not a guess. `$0` is the name the running
        // shell was invoked with, which is the program `invocation` chose.
        //
        // Exactly equal, not `ends_with`: every shell that can plausibly be
        // /bin/sh (dash, bash, ash, busybox, ksh, zsh) reports a $0 ending in
        // "sh", so a suffix check stays green even when `name()` says "sh" and
        // the process that ran the command line is bash. That is precisely the
        // divergence the system prompt would be lying about.
        let output = command("printf %s \"$0\"")
            .output()
            .expect("the platform shell must be runnable");
        assert!(output.status.success(), "{output:?}");
        let reported = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert_eq!(
            reported,
            program(),
            "the shell that ran the command line is not the one program() reports"
        );
        assert_eq!(display_name(&reported), name());
    }

    #[test]
    fn the_named_shell_resolves_to_an_executable_on_this_host() {
        // The other half of the same claim: a name the prompt hands the model
        // is worth nothing if nothing by that name can be spawned here. This
        // is also what `shebang()` writes into a script, so a host where it
        // does not resolve is a host where those tests would write a shebang
        // to a file that cannot start.
        let shebang = shebang();
        let path = shebang
            .strip_prefix("#!")
            .expect("a shebang starts with #!");
        let path = std::path::Path::new(path);
        assert!(path.is_absolute(), "{shebang}");
        assert!(
            crate::platform::exe_swap::is_executable(path),
            "{shebang} is not an executable file on this host"
        );
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(name()),
            "the resolved shell is not the one name() reports"
        );
    }

    #[test]
    fn a_bundled_shell_is_named_by_what_it_is() {
        assert_eq!(display_name("sh"), "sh");
        assert_eq!(display_name("/usr/bin/zsh"), "zsh");
        assert_eq!(display_name("/data/app/x/lib/arm64/libbash.so"), "bash");
        assert_eq!(display_name("/system/bin/sh"), "sh");
        assert_eq!(display_name("/opt/lib.so"), "lib.so");
        assert_eq!(display_name("/usr/lib/libfoo"), "libfoo");
    }
}
