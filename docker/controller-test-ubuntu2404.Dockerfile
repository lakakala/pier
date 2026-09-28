FROM ubuntu:24.04
ENV container=docker
RUN export DEBIAN_FRONTEND=noninteractive \
    && apt-get -o Acquire::Retries=3 update \
    && apt-get install -y --no-install-recommends systemd systemd-sysv dbus passwd libc-bin git python3 curl ca-certificates util-linux procps needrestart nginx openssl iptables iproute2 \
    && rm -rf /var/lib/apt/lists/*
# Container tests must not register or flush the host's binary interpreters.
RUN systemctl mask systemd-binfmt.service proc-sys-fs-binfmt_misc.automount proc-sys-fs-binfmt_misc.mount \
    && rm -f /usr/sbin/policy-rc.d
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]

# Docker runs only inside this disposable systemd container; no host socket.
# runc must execute natively on the shared kernel; QEMU cannot emulate its
# namespace/fork setup. Service binaries and build containers stay TARGETARCH.
ARG BUILDARCH
ARG DOCKER_VERSION=28.5.1
RUN case "$BUILDARCH" in amd64) machine=x86_64 ;; arm64) machine=aarch64 ;; *) exit 1 ;; esac \
    && curl --fail --location --retry 3 "https://download.docker.com/linux/static/stable/$machine/docker-$DOCKER_VERSION.tgz" -o /tmp/docker.tgz \
    && tar -xzf /tmp/docker.tgz -C /usr/local/bin --strip-components=1 \
    && rm /tmp/docker.tgz
