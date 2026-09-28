#!/usr/bin/env bash
set -euo pipefail
pier_arch=${1:?Usage: bash scripts/test-agent-packages.sh amd64|arm64}
case "$pier_arch" in amd64|arm64) ;; *) exit 2 ;; esac
pier_root=$(cd -- "$(dirname -- "$0")/.." && pwd)
pier_controller="$pier_root/target/services-$pier_arch/debug/pier-controller"
[ -f "$pier_controller" ] || { echo 'Run scripts/test-services.sh for this architecture first.' >&2; exit 2; }
pier_version=$(python3 "$pier_root/scripts/release.py" version)
pier_fixtures="$pier_root/target/package-upgrade-fixtures/$pier_arch"
mkdir -p "$pier_fixtures"
for pier_format in deb rpm; do
  docker run --rm --pull=never --platform "linux/$pier_arch" \
    -v "$pier_root:/src:ro" -v "$pier_root/target/agent-packages-$pier_arch/release:/binary:ro" -v "$pier_fixtures:/out" \
    "pier-agent-package-$pier_format:$pier_arch" bash /src/scripts/package-agent-inner.sh "$pier_format" "$pier_version" 2 "$pier_arch"
done
pier_container=
cleanup() { if [ -n "$pier_container" ]; then docker rm -f "$pier_container" >/dev/null; fi; }
trap cleanup EXIT
for pier_distro in ubuntu2404 almalinux8; do
  docker build --platform "linux/$pier_arch" -f "$pier_root/docker/agent-test-$pier_distro.Dockerfile" \
    -t "pier-agent-test-$pier_distro:$pier_arch" --build-arg http_proxy --build-arg https_proxy --build-arg no_proxy --build-arg NO_PROXY "$pier_root"
  # Private PID and cgroup namespaces; no host data or host cgroup bind mounts.
  pier_container=$(docker run -d --privileged --cgroupns=private --platform "linux/$pier_arch" \
    --tmpfs /run --tmpfs /run/lock --tmpfs /tmp \
    -v "$pier_root:/src:ro" -v "$pier_controller:/test/pier-controller:ro" \
    "pier-agent-test-$pier_distro:$pier_arch")
  docker exec "$pier_container" python3 /src/scripts/test-agent-packages.py "$pier_distro" "$pier_arch"
  docker restart -t 30 "$pier_container" >/dev/null
  docker exec "$pier_container" python3 /src/scripts/test-agent-packages.py "$pier_distro" "$pier_arch" --after-boot
  docker rm -f "$pier_container" >/dev/null
  pier_container=
done
