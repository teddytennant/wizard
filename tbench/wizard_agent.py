"""Terminal-Bench 2.x (Harbor) agent adapter for Wizard.

Run it with Harbor's custom-agent import path::

    harbor run -d terminal-bench/terminal-bench-2-1 \
        -a tbench.wizard_agent:WizardAgent \
        -m xai/grok-4.5 -k 1 -n 4

Wizard ships as a single static binary, so `install` uploads the one built by
`tbench/Dockerfile.build` rather than curling an installer inside the task
container. That keeps the benchmarked artifact byte-identical to the one we
built and makes runs reproducible: no network fetch mid-benchmark, and no
dependence on whatever the latest release happens to be that day.

Pinning the artifact is the point, but an unchecked pin rots: this adapter spent
months uploading a binary built from a branch that had long since stopped being
Wizard, and every score it produced described software nobody was shipping. So
`install` verifies the pin against the source tree it claims to represent before
uploading it, and refuses to benchmark a binary that has fallen behind. See
`_check_fresh`.
"""

from __future__ import annotations

import logging
import os
import shlex
import subprocess
import tomllib
from pathlib import Path
from typing import override

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

logger = logging.getLogger(__name__)

# Where the built binary lands (see tbench/README.md). Overridable so CI or a
# remote runner can point at an artifact it fetched elsewhere.
DEFAULT_BINARY = Path(__file__).parent / "dist" / "wizard"
BINARY_ENV = "WIZARD_TB_BINARY"

# The Wizard checkout the binary is expected to have been built from. This
# adapter lives on a long-lived branch that does not track Wizard's development
# line, so its own tree is the wrong default for anyone benchmarking current
# Wizard: point this at the checkout you actually develop in.
DEFAULT_SOURCE = Path(__file__).parent.parent
SOURCE_ENV = "WIZARD_TB_SOURCE"

# Escape hatch for the deliberate case — bisecting a regression, reproducing an
# old submission — where the binary is *meant* to disagree with the source.
ALLOW_STALE_ENV = "WIZARD_TB_ALLOW_STALE"

# Source files whose change invalidates a built binary. Everything else in the
# tree (docs, this adapter, CI config) can move without a rebuild.
SOURCE_INPUTS = ("src", "Cargo.toml", "Cargo.lock", "build.rs")

CONTAINER_BINARY = "/installed-agent/wizard"

# Wizard's own OAuth token store, used only as a fallback when no API key is
# present (see `_auth`).
DEFAULT_OAUTH_TOKEN = Path.home() / ".wizard" / "xai_oauth.json"
OAUTH_TOKEN_ENV = "WIZARD_TB_XAI_OAUTH"

# Harbor's `-m <provider>/<model>` provider -> the fields Wizard's config.toml
# needs. Wizard never persists a key, only the name of the env var holding it,
# so each entry names the var we inject into the container per-exec.
PROVIDERS = {
    "xai": ("xai", "https://api.x.ai/v1", "XAI_API_KEY"),
    "anthropic": ("anthropic", "https://api.anthropic.com", "ANTHROPIC_API_KEY"),
    "openai": ("openai", "https://api.openai.com/v1", "OPENAI_API_KEY"),
    "openrouter": ("openrouter", "https://openrouter.ai/api/v1", "OPENROUTER_API_KEY"),
}


class WizardAgent(BaseInstalledAgent):
    """Wizard driven headlessly in sovereign mode, one task per container."""

    @staticmethod
    @override
    def name() -> str:
        return "wizard"

    @override
    def version(self) -> str:
        return self._version or "dev"

    @override
    def get_version_command(self) -> str | None:
        return f"{CONTAINER_BINARY} --version"

    @override
    def parse_version(self, stdout: str) -> str:
        # `wizard --version` prints "wizard <semver>".
        return stdout.strip().removeprefix("wizard").strip() or "dev"

    def _source(self) -> Path:
        return Path(os.environ.get(SOURCE_ENV, DEFAULT_SOURCE))

    def _build_command(self) -> str:
        """The exact command that produces a binary matching the source tree.

        The build context is the *source* checkout, not this one. `Dockerfile.build`
        does `COPY . .`, so the context decides which Wizard gets compiled; passing
        it explicitly is what lets the adapter live on one branch and benchmark
        another.
        """
        dockerfile = Path(__file__).parent / "Dockerfile.build"
        dest = Path(__file__).parent / "dist"
        return (
            f"docker build -f {dockerfile} --target export "
            f"--output type=local,dest={dest} {self._source()}"
        )

    def _source_version(self, source: Path) -> str | None:
        """The version in the source tree's Cargo.toml, or None if unreadable."""
        manifest = source / "Cargo.toml"
        try:
            with manifest.open("rb") as fh:
                data = tomllib.load(fh)
        except (OSError, tomllib.TOMLDecodeError):
            return None
        for table in (data.get("package"), data.get("workspace", {}).get("package")):
            if isinstance(table, dict) and isinstance(table.get("version"), str):
                return table["version"]
        return None

    def _binary_version(self, binary: Path) -> str | None:
        """`wizard --version` run on the host, or None if it will not exec here.

        The artifact is a freestanding static binary with no PT_INTERP, so it runs
        on the host as readily as in a task container. If that ever stops being
        true the check degrades to the mtime comparison rather than blocking a run.
        """
        try:
            out = subprocess.run(
                [str(binary), "--version"],
                capture_output=True,
                text=True,
                timeout=30,
                check=True,
            ).stdout
        except (OSError, subprocess.SubprocessError):
            return None
        return out.strip().removeprefix("wizard").strip() or None

    @staticmethod
    def _same_version(a: str, b: str) -> bool:
        """Compare two Wizard versions, tolerating elided trailing zeros.

        `wizard --version` prints "1.1" where Cargo.toml carries "1.1.0", so the
        strings differ for every release ending in a zero. Comparing the padded
        numeric triple plus any pre-release suffix keeps "1.1" == "1.1.0" without
        also collapsing "2.0.0-rc1" into "2.0.0".
        """

        def parts(v: str) -> tuple[tuple[int, ...], str] | None:
            core, _, suffix = v.partition("-")
            core = core.partition("+")[0]
            fields = core.split(".")
            if not all(f.isdigit() for f in fields):
                return None
            padded = tuple(int(f) for f in fields) + (0,) * (3 - len(fields))
            return padded[:3], suffix

        pa, pb = parts(a), parts(b)
        if pa is None or pb is None:
            return a == b
        return pa == pb

    def _newer_than_binary(self, source: Path, binary: Path) -> list[Path]:
        """Source inputs modified since the binary was built."""
        built = binary.stat().st_mtime
        changed: list[Path] = []
        for name in SOURCE_INPUTS:
            path = source / name
            candidates = path.rglob("*") if path.is_dir() else [path]
            for candidate in candidates:
                try:
                    if candidate.is_file() and candidate.stat().st_mtime > built:
                        changed.append(candidate)
                except OSError:
                    # A dangling symlink in the tree is not evidence of a rebuild.
                    continue
        return changed

    def _check_fresh(self, binary: Path, source: Path) -> None:
        """Refuse to benchmark a binary that no longer represents `source`.

        Two independent signals, because neither alone is sufficient. The version
        strings disagreeing proves staleness across a release; mtimes catch the
        much commoner case of edits within one version, where `--version` cannot
        tell a rebuild from a stale artifact.
        """
        reasons: list[str] = []

        src_version = self._source_version(source)
        bin_version = self._binary_version(binary)
        if (
            src_version
            and bin_version
            and not self._same_version(src_version, bin_version)
        ):
            reasons.append(
                f"binary is Wizard {bin_version}, but {source}/Cargo.toml says "
                f"{src_version}"
            )

        if src_version is None:
            reasons.append(f"no readable Cargo.toml under {source}")
        else:
            changed = self._newer_than_binary(source, binary)
            if changed:
                sample = ", ".join(str(p.relative_to(source)) for p in changed[:3])
                more = f" (+{len(changed) - 3} more)" if len(changed) > 3 else ""
                reasons.append(
                    f"{len(changed)} source file(s) changed since the binary was "
                    f"built: {sample}{more}"
                )

        if not reasons:
            return

        detail = "\n".join(f"  - {r}" for r in reasons)
        if os.environ.get(ALLOW_STALE_ENV):
            logger.warning(
                "Benchmarking a stale Wizard binary because %s is set:\n%s",
                ALLOW_STALE_ENV,
                detail,
            )
            return

        raise RuntimeError(
            f"The Wizard binary at {binary} does not match the source at {source}:\n"
            f"{detail}\n"
            "Scoring it would describe software you are not shipping. Rebuild:\n"
            f"  {self._build_command()}\n"
            f"Point {SOURCE_ENV} at a different checkout, or set "
            f"{ALLOW_STALE_ENV}=1 if the mismatch is deliberate."
        )

    def _binary(self) -> Path:
        path = Path(os.environ.get(BINARY_ENV, DEFAULT_BINARY))
        if not path.is_file():
            raise FileNotFoundError(
                f"No Wizard binary at {path}. Build one with:\n"
                f"  {self._build_command()}\n"
                f"or point {BINARY_ENV} at an existing static build."
            )
        self._check_fresh(path, self._source())
        return path

    def _provider(self) -> tuple[str, str, str]:
        """(provider, base_url, key_env) from Harbor's `-m <provider>/<model>`."""
        if not self.model_name:
            raise ValueError("WizardAgent needs -m <provider>/<model>, e.g. xai/grok-4.5")
        if "/" not in self.model_name:
            raise ValueError(
                f"Model {self.model_name!r} must be <provider>/<model>, e.g. xai/grok-4.5"
            )
        provider, _ = self.model_name.split("/", 1)
        if provider not in PROVIDERS:
            raise ValueError(
                f"Provider {provider!r} is not wired up here. "
                f"Known: {', '.join(sorted(PROVIDERS))}"
            )
        _, base_url, key_env = PROVIDERS[provider]
        return provider, base_url, key_env

    def _model_tag(self) -> str:
        return self.model_name.split("/", 1)[1]

    def _oauth_token(self) -> Path | None:
        path = Path(os.environ.get(OAUTH_TOKEN_ENV, DEFAULT_OAUTH_TOKEN))
        return path if path.is_file() else None

    def _auth(self) -> tuple[str, str | None, Path | None]:
        """Resolve how Wizard will authenticate in-container.

        Returns (wizard_provider_kind, api_key, oauth_token_path).

        An API key is preferred whenever one is available. The `wizard --login
        xai` OAuth token is accepted as a fallback so a quick local baseline can
        run without minting a key, but it is a poor fit for a scoring run: every
        trial is its own container, and containers sharing one refresh token race
        to refresh it. Harbor records the resulting auth failures as reward 0,
        i.e. indistinguishable from Wizard genuinely failing the task. It is also
        unreproducible by anyone verifying a leaderboard submission.
        """
        provider, _, key_env = self._provider()
        key = self._get_env(key_env)
        if key:
            return provider, key, None

        if provider == "xai":
            token = self._oauth_token()
            if token:
                logger.warning(
                    "%s is unset; falling back to the OAuth token at %s. Fine for a "
                    "local baseline, but use an API key for any scored run — shared "
                    "refresh tokens race across trials and the failures score as 0.",
                    key_env,
                    token,
                )
                return "xaioauth", None, token

        raise ValueError(
            f"{key_env} is unset on the host. Export it, or pass --ae {key_env}=..., "
            "and Harbor will inject it into each task container."
        )

    def _config_toml(self) -> str:
        """Wizard's ~/.wizard/config.toml, pre-seeded so it never onboards.

        Without a config Wizard runs its first-run onboarding wizard, which would
        block forever in a container with no TTY.
        """
        provider, base_url, key_env = self._provider()
        kind, api_key, _ = self._auth()
        model = self._model_tag()

        lines = [
            f'model = "{model}"',
            'mode = "sovereign"',
            f'active_provider = "{provider}"',
            "",
            "[[providers]]",
            f'name = "{provider}"',
            f'kind = "{kind}"',
            f'base_url = "{base_url}"',
            f'model = "{model}"',
        ]
        # The OAuth provider reads ~/.wizard/xai_oauth.json and takes no key env.
        if api_key is not None:
            lines.append(f'api_key_env = "{key_env}"')
        lines += [
            "",
            # The startup release check would fire an outbound GitHub request from
            # every task container: pure noise, and a hard failure on the
            # network-restricted tasks. `auto` is off by default but is pinned here
            # too — a binary that self-updated mid-benchmark would mean we scored
            # something other than what we built.
            "[update]",
            "notify = false",
            "auto = false",
            "",
        ]
        return "\n".join(lines)

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        _, _, oauth_token = self._auth()

        await environment.upload_file(self._binary(), CONTAINER_BINARY)
        await self.exec_as_root(
            environment,
            f"chmod 0755 {CONTAINER_BINARY} "
            f"&& ln -sf {CONTAINER_BINARY} /usr/local/bin/wizard",
        )

        # Written as the agent user so it lands in the HOME that `run` will use.
        config = shlex.quote(self._config_toml())
        await self.exec_as_agent(
            environment,
            f'mkdir -p "$HOME/.wizard" && printf %s {config} > "$HOME/.wizard/config.toml"',
        )

        if oauth_token is not None:
            # Wizard reads (and refreshes) the token at ~/.wizard/xai_oauth.json.
            # Upload via root, then hand it to the agent user at 0600 — Wizard
            # tightens these permissions itself and will reject a lax token file.
            await environment.upload_file(oauth_token, "/tmp/xai_oauth.json")
            await self.exec_as_agent(
                environment,
                'mkdir -p "$HOME/.wizard" '
                '&& cp /tmp/xai_oauth.json "$HOME/.wizard/xai_oauth.json" '
                '&& chmod 0600 "$HOME/.wizard/xai_oauth.json" '
                "&& rm -f /tmp/xai_oauth.json",
            )

    @override
    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        _, _, key_env = self._provider()
        _, api_key, _ = self._auth()
        env = {key_env: api_key} if api_key else {}

        # No --loop / --max-hours cap: Harbor already bounds each trial with the
        # task's own timeout, and a tighter cap here would make Wizard give up on
        # long tasks it could otherwise finish, quietly depressing the score.
        command = (
            "wizard --mode sovereign --output-format text "
            f"-p {shlex.quote(instruction)} 2>&1 | tee /logs/agent/wizard.txt"
        )
        await self.exec_as_agent(environment, command, env=env)
