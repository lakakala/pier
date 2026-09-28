# Build with --platform linux/amd64 or linux/arm64.
# Tools run natively in the target architecture, using QEMU when necessary.
FROM almalinux:8.10
RUN dnf install -q -y ca-certificates curl gcc gcc-c++ glibc-devel make git tar gzip xz binutils \
    && dnf clean all

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
