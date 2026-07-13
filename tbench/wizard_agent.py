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
"""

from __future__ import annotations

import logging
import os
import shlex
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

    def _binary(self) -> Path:
        path = Path(os.environ.get(BINARY_ENV, DEFAULT_BINARY))
        if not path.is_file():
            raise FileNotFoundError(
                f"No Wizard binary at {path}. Build one with:\n"
                "  docker build -f tbench/Dockerfile.build --target export "
                "--output type=local,dest=tbench/dist .\n"
                f"or point {BINARY_ENV} at an existing static build."
            )
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
