FROM wizard-bench-base
# README: npm install -g @openai/codex (needs Node 22+; NodeSource is the
# usual way to get one on Ubuntu). Node is counted as part of the install.
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/* \
    && npm install -g @openai/codex \
    && npm cache clean --force
