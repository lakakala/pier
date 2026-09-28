# Build with --platform linux/amd64 or linux/arm64.
# Tools run natively in the target architecture, using QEMU when necessary.
FROM ubuntu:24.04
# Content-addressed indexes avoid stale proxy caches during mirror updates.
RUN export DEBIAN_FRONTEND=noninteractive \
    && apt-get -o Acquire::Retries=3 -o Acquire::By-Hash=force -o Acquire::http::No-Cache=true update -qq \
    && apt-get -o Acquire::Retries=3 install -qq -y --no-install-recommends ca-certificates curl build-essential git tar gzip xz-utils binutils \
    && rm -rf /var/lib/apt/lists/*

ARG RUST_VERSION=1.98.1
ENV RUSTUP_HOME=/opt/rustup
ENV CARGO_HOME=/opt/cargo
ENV PATH=/opt/cargo/bin:${PATH}
RUN curl --fail --silent --show-error --location --retry 3 https://sh.rustup.rs -o /tmp/rustup-init.sh \
    && sh /tmp/rustup-init.sh -y --no-modify-path --profile minimal --default-toolchain "${RUST_VERSION}" \
    && chmod -R a+rX /opt/rustup /opt/cargo \
    && rustc --version \
    && cargo --version \
    && rm -f /tmp/rustup-init.sh
WORKDIR /src
