// Type declarations for a Wizard plugin written in JavaScript or TypeScript.
//
// A plugin is a directory under `~/.wizard/plugins/<name>/` holding
// `manifest.toml` and `plugin.js`. The script is loaded as an ES module and
// must default-export an object with a `name` and an `apply`:
//
//     /// <reference path="wizard-plugin.d.ts" />
//     export default {
//       name: "hello",
//       apply(ctx) {
//         ctx.tool({
//           name: "hello",
//           description: "Say hello",
//           execute: () => "hi",
//         });
//       },
//     };
//
// # Writing it in TypeScript
//
// Wizard ships no TypeScript compiler — types are erased at build time, so
// there is nothing for the runtime to do with them, and the smallest Rust
// crate that could strip them is several times the size of the whole
// JavaScript engine. Compile before you install:
//
//     esbuild plugin.ts --bundle --format=esm --platform=neutral \
//         --external:wizard --outfile=plugin.js
//
// `tsc --module es2022 --target es2022 plugin.ts` works too, for a single file
// with no imports. `--bundle` matters if you have more than one file: there is
// no module loader in the VM, so `import "./other.js"` fails at runtime and
// everything a plugin uses has to be in `plugin.js`.
//
// # Type-checking plain JavaScript
//
// If you would rather not compile anything, put the reference comment at the
// top of `plugin.js`, annotate with JSDoc, and run
// `tsc --noEmit --checkJs --target es2022 plugin.js`. That is how
// `src/plugins/js/json/plugin.js` — the plugin Wizard ships — is written, and
// it is why that file has no build step.
//
// # What is not here
//
// No `require`, no `import`, no `process`, no `fetch`, no `setTimeout`. A
// plugin's whole surface is `ctx` and `wizard`, and `wizard` carries only the
// namespaces the manifest's `capabilities` declared. Everything else about the
// sandbox is in `src/kernel/js/host.rs`.
//
// `console.log`, `.info`, `.debug`, `.warn` and `.error` do exist, and go to
// Wizard's log rather than to a terminal. They are not declared here because
// TypeScript's own lib already has them and a second declaration collides with
// it; the signatures are the ones you expect.
//
// This file is a *global* declaration file — no top-level `import` or `export`
// — so a `/// <reference path=...>` comment is all it takes to see `ctx`,
// `wizard` and every type below. Adding an `export` to it would turn it into a
// module and take all of that back out of scope, which is the one edit to
// resist.

/** JSON, as it crosses the plugin boundary in both directions. */
type Json =
  | null
  | boolean
  | number
  | string
  | Json[]
  | { [key: string]: Json };

/** A lifecycle event a plugin can subscribe to or publish. */
type WizardEvent =
  | "session_start"
  | "session_end"
  | "user_prompt"
  | "turn_start"
  | "turn_end"
  | "pre_tool_use"
  | "post_tool_use"
  | "pre_model_call"
  | "post_model_call"
  | "compaction"
  | "checkpoint"
  | "plugin_loaded"
  | "plugin_unloaded"
  | "config_reload";

/** Where a slash command is allowed to run. */
type CommandSurface = "tui" | "gui" | "gateway";

/**
 * How much a tool is allowed to do, which is what plan mode gates on.
 *
 * Anything other than `"read_only"` or `"edit"` — including leaving it out —
 * is read as `"execute"`, the conservative answer. A tool that only reads
 * should say so, or plan mode will refuse it.
 */
type ToolAccess = "read_only" | "edit" | "execute";

/**
 * What a tool body is told about the call, beyond its arguments.
 *
 * One field, and that is not an oversight: the rest of Wizard's tool context
 * is Rust handles — an event channel, a cancel handle, task registries — that
 * reach a plugin through `wizard.*` if they reach it at all. `cwd` is
 * different, because it is what every path-taking tool resolves against.
 */
interface ToolCall {
  /** The directory this call is about. Resolve relative paths against it. */
  readonly cwd: string;
}

/**
 * What a tool body may return.
 *
 * A string is the content. `undefined` or `null` is empty content. Anything
 * else is serialized as JSON, so returning an object is a fine way to answer
 * with structured data.
 *
 * `{ content, is_error: true }` is the spelling for "this worked and the news
 * is bad" — a command that exited non-zero, a file that is not there. Throwing
 * is different and means the tool itself broke. The distinction reaches the
 * model, so it is worth getting right.
 */
type ToolResult =
  | string
  | number
  | boolean
  | null
  | undefined
  | Json[]
  | { content: string; is_error?: boolean }
  | { [key: string]: Json };

interface ToolSpec {
  /** The name the model calls. Must not collide with a built-in tool. */
  name: string;
  description?: string;
  /**
   * JSON Schema for the arguments. Leave it out for a tool that takes none —
   * the host supplies the empty object schema every provider expects.
   */
  parameters?: Json;
  access?: ToolAccess;
  /** May be `async`; a returned promise is awaited before the model sees it. */
  execute: (args: any, call: ToolCall) => ToolResult | Promise<ToolResult>;
}

interface CommandSpec {
  /** Typed as `/name`. A name a built-in owns is refused at registration. */
  name: string;
  description?: string;
  /** Argument hint shown in the palette, e.g. `"<branch>"`. */
  args?: string;
  /**
   * Surfaces this command runs on. Absent means all of them; present and
   * empty means none, which is almost certainly a mistake rather than a
   * shorthand.
   */
  surfaces?: CommandSurface[];
  /** May be `async`. Whatever it returns is printed to the transcript. */
  run: (args: string) => string | Promise<string>;
}

/** What `ctx.emit` reports back about a dispatch. */
interface Dispatch {
  /** The payload after every handler that rewrote it. */
  payload: Json;
  vetoed: boolean;
  /** The veto reason, or `null` when nothing vetoed. */
  veto: string | null;
  /** The plugin that vetoed, when one did. */
  veto_by?: string;
  /** How many handlers ran. */
  ran: number;
  /** How many handlers threw. A handler that throws is skipped, not fatal. */
  failures: number;
}

/**
 * What an event handler may return.
 *
 * Nothing (or `undefined`) observes. `{ payload }` rewrites the payload for
 * every handler after this one and for the caller. `{ veto }` refuses the
 * whole thing, naming a reason a person will read. An object with neither key
 * is treated as an observation, so `return {}` from a handler that meant
 * nothing by it does not blank the payload.
 */
type Verdict =
  | void
  | undefined
  | { payload: Json }
  | { veto: string };

/**
 * The plugin API. One shape across all three backends — Rust, Lua and
 * JavaScript — so a plugin can be ported between them without being
 * redesigned.
 */
interface PluginContext {
  /** Register a tool the model can call. */
  tool(spec: ToolSpec): void;

  /** Register a slash command. */
  command(spec: CommandSpec): void;

  /**
   * Always throws.
   *
   * It exists so the API is the same shape in every language, and refuses
   * because a provider is TLS and SSE framing — the half of Wizard that stays
   * in Rust. A plugin that wants to add a backend adds a Rust one.
   */
  provider(spec: unknown): never;

  /**
   * Subscribe to a lifecycle event. Lower priority runs first; the default is
   * 0 and the type is signed, so a handler can order itself ahead of
   * everything without knowing how many others exist.
   */
  on(
    event: WizardEvent,
    handler: (event: WizardEvent, payload: Json) => Verdict | Promise<Verdict>,
    priority?: number,
  ): void;

  /** Publish an event and wait for every handler. */
  emit(event: WizardEvent, payload?: Json): Promise<Dispatch>;

  /**
   * Expose a value to other plugins under a name.
   *
   * A snapshot, not a live object: what other plugins get is the JSON as it
   * was when this ran. There is no way for a plugin to expose something core
   * can call.
   */
  provide(name: string, value: Json): void;

  /**
   * Take another plugin's service, or `undefined` when nothing provides it.
   *
   * `undefined` is the composability rule rather than a failure: ask, and
   * degrade when the answer is nothing. A service provided by a *Rust* plugin
   * is also `undefined` here — JavaScript cannot call a Rust object.
   */
  inject(name: string): Json | undefined;

  /**
   * Load a child plugin from a directory beside this one, and hand it a config
   * slice. The child is unloaded with its parent. Takes a name, not a path.
   */
  plugin(name: string, config?: Json): Promise<string>;

  /**
   * Register a teardown, run at unload after every registration is gone,
   * newest first. This is where an open handle or a temp directory goes.
   */
  effect(dispose: () => void | Promise<void>, label?: string): void;

  /** This plugin's slice of `[plugins]` in `config.toml`. */
  config(): Json;

  /** This plugin's name, as the manifest spells it. */
  name(): string;
}

/** `wizard.fs` — read and write files. Always present. */
interface WizardFs {
  /**
   * Read a file as text.
   *
   * Without the `filesystem` capability this is confined to the project
   * directory: an absolute path, a leading `~` and a `..` that climbs out are
   * all refused, and a symlink that points out of the project is refused after
   * resolution. With the capability, paths resolve as written.
   */
  read(path: string): string;
  /** Write a file, creating parent directories. Same confinement as `read`. */
  write(path: string, contents: string): void;
}

/** `wizard.limits` — the byte budgets a native tool applies to its answer. */
interface WizardLimits {
  /** The blanket cap on any tool's output. */
  output: number;
  /** What a diff gets. Smaller than `output`, on purpose. */
  diff: number;
  /** What search results get. */
  search: number;
  /** What a directory listing gets. */
  listing: number;
  /** What an error message gets. Small — stderr this long is not a message. */
  error: number;
}

/** `wizard.http` — present only with the `network` capability. */
interface WizardHttp {
  /** GET a URL and return the body as text. Redirects are followed. */
  get(url: string): Promise<string>;
  /** POST a body. Redirects are *not* followed, to avoid replaying it. */
  post(url: string, body?: string): Promise<string>;
  /** PUT a body. Redirects are not followed. */
  put(url: string, body?: string): Promise<string>;
}

/** `wizard.model` — present only with the `model` capability. */
interface WizardModel {
  /**
   * One completion, on the user's account, billed to them.
   *
   * Refuses when no agent is bound — a plugin-only process, `mcp serve`, a
   * unit test — rather than quietly building a provider whose spend would
   * never reach `/cost`.
   */
  complete(prompt: string): Promise<string>;
}

/** `wizard.ui` — present only with the `ui` capability. */
interface WizardUi {
  /**
   * Write a line to the transcript. With no turn in front of it this goes to
   * the log rather than failing: a notice's failure mode is nobody hearing it.
   */
  notify(text: string): Promise<void>;
}

/** `wizard.agent` — present only with the `agent` capability. */
interface WizardAgent {
  /** Start a subagent, wait for it, and return what it said. */
  spawn(task: string): Promise<string>;
}

/** One program to run through `wizard.process.exec`. */
interface ExecRequest {
  /** Program and arguments. Never a shell line — there is nothing to quote. */
  argv: string[];
  /** Where to run it. Defaults to the host's working directory. */
  cwd?: string;
  /** Budget before the whole process group is killed. */
  timeout_ms?: number;
}

/** What a program did. No verdict — the plugin decides what a code means. */
interface ExecOutcome {
  stdout: string;
  stderr: string;
  /**
   * Exit status, or `null` when the process was signalled or timed out.
   * A program that is not installed exits 127, one that would not run 126 —
   * the shell's own codes, so a plugin can tell "missing" from "broken".
   */
  code: number | null;
  /** Seconds after which it was killed, or `null`. Set instead of `code`. */
  timed_out: number | null;
}

/** `wizard.process` — present only with the `process` capability. */
interface WizardProcess {
  /**
   * Run a shell line and return its output, throwing if it failed.
   *
   * Right for "do this and tell me if it worked". For anything that branches
   * on an exit code or reads stderr separately, use `exec`.
   */
  run(command: string): Promise<string>;
  /** Run one program by argv and hand back what it did. */
  exec(request: ExecRequest): Promise<ExecOutcome>;
}

/** `wizard.paths` — present only with the `filesystem` capability. */
interface WizardPaths {
  /** The project root. What a confined `wizard.fs` is confined to. */
  project: string;
  /** `~/.wizard`, or wherever it is redirected to under `cargo test`. */
  home?: string;
  /** The source checkout deep evolve builds from. */
  source?: string;
  /** The evolution log. */
  evolution_log?: string;
}

/**
 * The host surface.
 *
 * Everything gated is *absent* rather than present-and-refusing, so
 * `if (wizard.http)` is how a plugin asks whether it may fetch. A namespace
 * that existed and threw would make that a question you could only answer by
 * catching.
 */
interface Wizard {
  /** This plugin's name. */
  readonly plugin: string;
  /** `"quickjs"`. The Lua backend answers `"luajit"` to the same question. */
  readonly runtime: string;
  readonly fs: WizardFs;
  readonly limits: WizardLimits;
  /**
   * Trim text to a byte budget with the same head/tail framing a native tool
   * uses, spilling the full text to the session's scratch file when it is long
   * enough to be worth keeping.
   */
  truncate(text: string, maxBytes?: number): string;
  /** Park for up to a minute. Costs no CPU and does not extend the deadline. */
  sleep(millis: number): Promise<void>;
  /** Write a line to Wizard's log at info level. */
  log(message: string): void;

  readonly http?: WizardHttp;
  readonly model?: WizardModel;
  readonly ui?: WizardUi;
  readonly agent?: WizardAgent;
  readonly process?: WizardProcess;
  readonly paths?: WizardPaths;
}

/**
 * What a `plugin.js` default-exports.
 *
 * `WizardPlugin` rather than `Plugin` because these declarations are global,
 * and the DOM lib already has a global `Plugin` (the old `navigator.plugins`
 * entry). Two global interfaces with one name are *merged* by TypeScript, so
 * the collision is not a shadowing but a silent mixture of two unrelated
 * shapes — which is worth one prefix to avoid.
 */
interface WizardPlugin {
  /** Should match `name` in `manifest.toml`. */
  name: string;
  /** May be `async`; the load waits for it. */
  apply: (ctx: PluginContext) => void | Promise<void>;
}

/** The host surface. Present in every plugin. */
declare const wizard: Wizard;

/**
 * The same object `apply` is handed. Reading it from the global is supported;
 * taking the argument is clearer.
 */
declare const ctx: PluginContext;
