FROM wizard-bench-base
# README: curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
# WIZARD_VERSION pins the release install.sh downloads (run.sh passes it through).
ARG WIZARD_VERSION=v3.0.1
RUN curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh \
    | WIZARD_VERSION=$WIZARD_VERSION bash
