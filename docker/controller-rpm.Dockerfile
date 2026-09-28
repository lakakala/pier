FROM almalinux:8.10
RUN dnf install -y rpm-build systemd binutils python3 \
    && dnf clean all
WORKDIR /build
