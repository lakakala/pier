# Build with --platform linux/amd64 or linux/arm64.
# Tools run natively in the target architecture, using QEMU when necessary.
FROM ubuntu:24.04
# Content-addressed indexes avoid stale proxy caches during mirror updates.
RUN export DEBIAN_FRONTEND=noninteractive \
    && apt-get -o Acquire::Retries=3 -o Acquire::By-Hash=force -o Acquire::http::No-Cache=true update -qq \
    && apt-get -o Acquire::Retries=3 install -qq -y --no-install-recommends ca-certificates curl build-essential git tar gzip xz-utils binutils \
    && rm -rf /var/lib/apt/lists/*

ARG GO_VERSION=1.27.1
# From https://go.dev/dl/?mode=json; update together with GO_VERSION.
ARG GO_SHA256_AMD64=63d339f0da5ab53635a56f2490a7984dfe12dfcff22ad749f63edaf590168445
ARG GO_SHA256_ARM64=3450b45a3f9ee8568792736a5c5e70a1f2e9b36c35a8f74958c03e51d7d92bec
ARG TARGETARCH
ENV PATH=/usr/local/go/bin:${PATH}
RUN test "${TARGETARCH}" = amd64 -o "${TARGETARCH}" = arm64 \
    && curl --fail --silent --show-error --location --retry 3 "https://go.dev/dl/go${GO_VERSION}.linux-${TARGETARCH}.tar.gz" -o /tmp/go.tar.gz \
    && if [ "${TARGETARCH}" = amd64 ]; then printf '%s  /tmp/go.tar.gz\n' "${GO_SHA256_AMD64}"; else printf '%s  /tmp/go.tar.gz\n' "${GO_SHA256_ARM64}"; fi | sha256sum -c - \
    && tar -C /usr/local -xzf /tmp/go.tar.gz \
    && go version \
    && rm -f /tmp/go.tar.gz
WORKDIR /src
