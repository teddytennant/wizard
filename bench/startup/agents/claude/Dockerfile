FROM wizard-bench-base
# README: curl -fsSL https://claude.ai/install.sh | bash   (a version argument pins it)
ARG CLAUDE_VERSION=2.1.268
RUN curl -fsSL https://claude.ai/install.sh | bash -s "$CLAUDE_VERSION"
ENV PATH="/root/.local/bin:${PATH}"
