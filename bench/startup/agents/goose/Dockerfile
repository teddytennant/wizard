FROM wizard-bench-base
# README: curl -fsSL https://github.com/block/goose/releases/download/stable/download_cli.sh | CONFIGURE=false bash
ARG GOOSE_VERSION=v1.50.0
RUN curl -fsSL https://github.com/block/goose/releases/download/stable/download_cli.sh \
    | CONFIGURE=false GOOSE_VERSION="$GOOSE_VERSION" bash
ENV PATH="/root/.local/bin:${PATH}"
