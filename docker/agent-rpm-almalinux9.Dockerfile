FROM almalinux:9.8
RUN dnf clean all \
    && dnf --refresh makecache \
    && dnf install -y rpm-build systemd systemd-rpm-macros binutils python3 \
    && dnf clean all
WORKDIR /build
