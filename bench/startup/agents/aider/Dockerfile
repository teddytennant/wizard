FROM wizard-bench-base
# README: curl -LsSf https://aider.chat/install.sh | sh. That script installs
# uv and runs `uv tool install --force --python python3.12 --with pip
# aider-chat@latest`; the same two steps here, with the version pinned.
ARG AIDER_VERSION=0.86.2
RUN curl -LsSf https://astral.sh/uv/install.sh | sh \
    && /root/.local/bin/uv tool install --force --python python3.12 --with pip "aider-chat==${AIDER_VERSION}"
ENV PATH="/root/.local/bin:${PATH}"
