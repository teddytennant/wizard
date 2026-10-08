# HOL Guard hook

Wizard's `pre_tool_use` hook can block a tool call before execution. HOL Guard can use that boundary to inspect shell commands without changing Wizard's default policy.

Install HOL Guard separately, then point a global Wizard hook at the bundled adapter:

```toml
[[hooks]]
event = "pre_tool_use"
matcher = "execute"
command = "python3 /path/to/wizard/contrib/hol-guard-hook.py"
timeout_secs = 15
```

The adapter passes `args.command` to `hol-guard command test <command> --json`. It returns exit 0 only when `classification.explicitly_benign` is `true` and `minimum_action` is `allow`. Review, risky, unknown, malformed output, a missing Guard executable, and Guard failures return exit 2, which Wizard blocks before the `execute` tool runs.

Keep the Wizard hook timeout above the adapter's 10 second Guard timeout. Wizard treats its own hook timeout as non-blocking, so the adapter resolves Guard uncertainty to exit 2 before that outer timeout expires.

This is command inspection rather than full HOL Guard runtime policy. It does not claim approvals, receipts, or protection for non-`execute` tools.

See [Lifecycle hooks](hooks.md) for hook loading, project trust, and exit-code semantics.
