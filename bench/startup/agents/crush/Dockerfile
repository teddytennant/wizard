FROM wizard-bench-base
# README (Debian/Ubuntu): the charm apt repo
ARG CRUSH_VERSION=0.93.1
RUN mkdir -p /etc/apt/keyrings \
    && curl -fsSL https://repo.charm.sh/apt/gpg.key | gpg --dearmor -o /etc/apt/keyrings/charm.gpg \
    && echo "deb [signed-by=/etc/apt/keyrings/charm.gpg] https://repo.charm.sh/apt/ * *" > /etc/apt/sources.list.d/charm.list \
    && apt-get update && apt-get install -y --no-install-recommends "crush=${CRUSH_VERSION}" \
    && rm -rf /var/lib/apt/lists/*
