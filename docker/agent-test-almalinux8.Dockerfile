FROM almalinux:8.10
ENV container=docker
RUN dnf clean all \
    && dnf --refresh makecache \
    && dnf install -y systemd dbus shadow-utils glibc-common git python3 curl util-linux procps-ng nginx openssl \
    && dnf clean all
# Container tests must not register or flush the host's binary interpreters.
RUN systemctl mask systemd-binfmt.service proc-sys-fs-binfmt_misc.automount proc-sys-fs-binfmt_misc.mount
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
