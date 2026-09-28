#!/usr/bin/env bash
set -euo pipefail

# Existing Docker images and QEMU/binfmt are prerequisites. This script never
# creates users on the host or changes the Docker daemon configuration.
pier_arch=${1:?Usage: scripts/test-services.sh amd64|arm64}
case "$pier_arch" in amd64|arm64) ;; *) echo "Unsupported architecture" >&2; exit 2 ;; esac
pier_repository=$(cd -- "$(dirname -- "$0")/.." && pwd)
pier_registry=${CARGO_HOME:-${HOME}/.cargo}/registry
pier_output="$pier_repository/target/services-$pier_arch"
pier_cache="$pier_repository/target/services-cache-$pier_arch"
mkdir -p "$pier_output" "$pier_cache"

docker run --rm --pull=never --platform "linux/$pier_arch" \
  --user "$(id -u):$(id -g)" \
  -e CARGO_HOME=/cache -e CARGO_TARGET_DIR=/out -e CARGO_BUILD_JOBS=4 \
  -e CARGO_PROFILE_DEV_DEBUG=0 -e CARGO_PROFILE_TEST_DEBUG=0 \
  -v "$pier_repository:/src:ro" -v "$pier_output:/out" \
  -v "$pier_cache:/cache" -v "$pier_registry:/cache/registry:ro" \
  pier-builder-rust:almalinux8 /bin/sh -ec \
  'cargo build --workspace --offline --locked; cargo test -p pier-agent --test lifecycle --no-run --offline --locked --message-format=json > /out/lifecycle-build.json'

# Select the executable Cargo just built, even if older hashes remain in deps/.
pier_test_binary=$(sed -n 's/.*"executable":"\([^"]*\/deps\/lifecycle-[^"]*\)".*/\1/p' "$pier_output/lifecycle-build.json")
case "$pier_test_binary" in
  /out/debug/deps/lifecycle-*) pier_test_binary="/binaries/${pier_test_binary#/out/debug/}" ;;
  *) echo "Lifecycle test binary not found in Cargo output" >&2; exit 1 ;;
esac

for pier_image in pier-builder-rust:almalinux8 pier-builder-rust:ubuntu24.04; do
  docker run --rm --pull=never --platform "linux/$pier_arch" \
    -e PIER_PRIVILEGED_TESTS=1 \
    -e PIER_CONTROLLER_BIN=/binaries/pier-controller \
    -e PIER_AGENT_BIN=/binaries/pier-agent \
    -v "$pier_output/debug:/binaries:ro" "$pier_image" \
    "$pier_test_binary" --ignored --nocapture
done
