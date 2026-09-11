FROM wizard-bench-base
# README: curl -fsSL https://opencode.ai/install | bash   (--version pins it)
ARG OPENCODE_VERSION=1.18.30
RUN curl -fsSL https://opencode.ai/install | bash -s -- --version "$OPENCODE_VERSION"
ENV PATH="/root/.opencode/bin:${PATH}"
