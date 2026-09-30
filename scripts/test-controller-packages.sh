#!/usr/bin/env bash
set -euo pipefail
pier_arch=${1:?Usage: bash scripts/test-controller-packages.sh amd64|arm64}
case "$pier_arch" in amd64|arm64) ;; *) exit 2 ;; esac
pier_root=$(cd -- "$(dirname -- "$0")/.." && pwd)
pier_container=
cleanup() {
  if [ -n "$pier_container" ]; then docker rm -f "$pier_container" >/dev/null; fi
}
trap cleanup EXIT
source "$pier_root/scripts/package-target.sh"
for pier_system in ubuntu24.04 almalinux8 almalinux9; do
  pier_package_target "$pier_system"
  pier_fixtures="$pier_root/target/controller-package-upgrade-fixtures/$pier_system/$pier_arch"
  bash "$pier_root/scripts/prepare-upgrade-fixtures.sh" controller "$pier_arch" 2 "$pier_fixtures" --system "$pier_system" --agent-packages "$pier_root/dist"
  docker build --platform "linux/$pier_arch" -f "$pier_root/docker/controller-test-$pier_distro.Dockerfile" \
    -t "pier-controller-test-$pier_distro:$pier_arch" --build-arg http_proxy --build-arg https_proxy --build-arg no_proxy --build-arg NO_PROXY "$pier_root"
  pier_container=$(docker run -d --privileged --cgroupns=private --platform "linux/$pier_arch" \
    --tmpfs /run --tmpfs /run/lock --tmpfs /tmp -v "$pier_root:/src:ro" -v "$pier_fixtures:/fixtures:ro" -e PIER_TEST_FIXTURES=/fixtures \
    "pier-controller-test-$pier_distro:$pier_arch")
  if ! docker exec "$pier_container" python3 /src/scripts/test-controller-packages.py "$pier_distro" "$pier_arch"; then
    docker exec "$pier_container" journalctl -u pier-controller --no-pager -n 80 || true
    docker exec "$pier_container" tail -80 /var/lib/pier-controller-package-test/docker.log || true
    exit 1
  fi
  docker restart -t 30 "$pier_container" >/dev/null
  docker exec "$pier_container" python3 /src/scripts/test-controller-packages.py "$pier_distro" "$pier_arch" --after-boot
  docker rm -f "$pier_container" >/dev/null
  pier_container=
done
