#!/usr/bin/env bash
# Execute prepared packages; fixture generation can happen on separate runners.
set -euo pipefail
pier_root=$(cd -- "$(dirname -- "$0")/.." && pwd)
case "${1:-all}" in
  amd64) pier_targets=amd64 ;;
  arm64) pier_targets=arm64 ;;
  all) pier_targets='amd64 arm64' ;;
  *) echo 'Usage: test-agent-auto-upgrade.sh [amd64|arm64|all] [--revision N] [--packages DIR] [--fixtures DIR] [--legacy-packages DIR] [--logs DIR]' >&2; exit 2 ;;
esac
if [ "$#" -gt 0 ]; then shift; fi
pier_revision=1
pier_packages="$pier_root/dist"
pier_fixtures="$pier_root/target/controller-package-upgrade-fixtures"
pier_legacy="$pier_root/target/fixtures/legacy-agent"
pier_logs="$pier_root/target/native-test-logs"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --revision) pier_revision=${2:?revision}; shift 2 ;;
    --packages) pier_packages=${2:?package directory}; shift 2 ;;
    --fixtures) pier_fixtures=${2:?fixture directory}; shift 2 ;;
    --legacy-packages) pier_legacy=${2:?legacy package directory}; shift 2 ;;
    --logs) pier_logs=${2:?log directory}; shift 2 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done
python3 "$pier_root/scripts/release.py" metadata --revision "$pier_revision" >/dev/null
pier_packages=$(cd -- "$pier_packages" && pwd)
pier_fixtures=$(cd -- "$pier_fixtures" && pwd)
pier_legacy=$(cd -- "$pier_legacy" && pwd)
mkdir -p "$pier_logs"
pier_logs=$(cd -- "$pier_logs" && pwd)
pier_container=
save_logs() {
  docker exec "$pier_container" journalctl --no-pager -n 500 >"$pier_logs/$pier_arch-$pier_distro-journal.log" 2>&1 || true
  docker logs "$pier_container" >"$pier_logs/$pier_arch-$pier_distro-container.log" 2>&1 || true
  docker exec "$pier_container" cat /var/lib/pier-controller-package-test/docker.log >"$pier_logs/$pier_arch-$pier_distro-docker.log" 2>&1 || true
}
cleanup() {
  pier_status=$?
  if [ -n "$pier_container" ]; then
    save_logs
    docker rm -f "$pier_container" >/dev/null || true
  fi
  exit "$pier_status"
}
trap cleanup EXIT
for pier_arch in $pier_targets; do
  test -d "$pier_fixtures/$pier_arch"
  for pier_distro in ubuntu2404 almalinux8 almalinux9; do
    docker build --platform "linux/$pier_arch" -f "$pier_root/docker/controller-test-$pier_distro.Dockerfile" \
      -t "pier-controller-test-$pier_distro:$pier_arch" --build-arg http_proxy --build-arg https_proxy --build-arg no_proxy --build-arg NO_PROXY "$pier_root"
    pier_container=$(docker run -d --privileged --cgroupns=private --platform "linux/$pier_arch" \
      --tmpfs /run --tmpfs /run/lock --tmpfs /tmp -v "$pier_root:/src:ro" \
      -v "$pier_packages:/packages:ro" -v "$pier_fixtures/$pier_arch:/fixtures:ro" \
      -v "$pier_legacy:/legacy-packages:ro" -e PIER_TEST_LEGACY_PACKAGES=/legacy-packages \
      -e PIER_TEST_REVISION="$pier_revision" -e PIER_TEST_PACKAGES=/packages -e PIER_TEST_FIXTURES=/fixtures \
      "pier-controller-test-$pier_distro:$pier_arch")
    docker exec "$pier_container" python3 /src/scripts/test-controller-packages.py "$pier_distro" "$pier_arch" --auto-upgrade
    docker restart -t 30 "$pier_container" >/dev/null
    docker exec "$pier_container" python3 /src/scripts/test-controller-packages.py "$pier_distro" "$pier_arch" --after-boot
    save_logs
    docker rm -f "$pier_container" >/dev/null
    pier_container=
  done
done
