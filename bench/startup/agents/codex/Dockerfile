FROM wizard-bench-base
# README: npm install -g @openai/codex (needs Node 22+; NodeSource is the
# usual way to get one on Ubuntu). Node is counted as part of the install.
ARG CODEX_VERSION=0.154.0
ARG NODE_MAJOR=22
RUN curl -fsSL https://deb.nodesource.com/setup_${NODE_MAJOR}.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/* \
    && npm install -g "@openai/codex@${CODEX_VERSION}" \
    && npm cache clean --force
