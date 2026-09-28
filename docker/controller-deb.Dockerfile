FROM ubuntu:24.04
RUN export DEBIAN_FRONTEND=noninteractive \
    && apt-get -o Acquire::Retries=3 update \
    && apt-get install -y --no-install-recommends debhelper dpkg-dev binutils python3 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
RUN export DEBIAN_FRONTEND=noninteractive \
    && apt-get -o Acquire::Retries=3 update \
    && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/*
