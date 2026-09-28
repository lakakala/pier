FROM ubuntu:24.04
ENV container=docker
RUN export DEBIAN_FRONTEND=noninteractive \
    && apt-get -o Acquire::Retries=3 update \
    && apt-get install -y --no-install-recommends systemd systemd-sysv dbus passwd libc-bin git python3 curl ca-certificates util-linux procps needrestart nginx openssl \
    && rm -rf /var/lib/apt/lists/*
# Container tests must not register or flush the host's binary interpreters.
RUN systemctl mask systemd-binfmt.service proc-sys-fs-binfmt_misc.automount proc-sys-fs-binfmt_misc.mount \
    && rm -f /usr/sbin/policy-rc.d
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
