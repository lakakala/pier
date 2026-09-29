#!/usr/bin/env bash
# Repackage an already compiled binary; each runner prepares only its own architecture.
set -euo pipefail
pier_service=${1:?Usage: prepare-upgrade-fixtures.sh agent|controller amd64|arm64 REVISION OUTPUT [AGENT_PACKAGES]}
pier_arch=${2:?architecture}
pier_revision=${3:?revision}
pier_output=${4:?output directory}
case "$pier_service" in agent|controller) ;; *) exit 2 ;; esac
case "$pier_arch" in amd64|arm64) ;; *) exit 2 ;; esac
[[ "$pier_revision" =~ ^[1-9][0-9]*$ ]] || { echo 'revision must be a positive integer' >&2; exit 2; }
pier_root=$(cd -- "$(dirname -- "$0")/.." && pwd)
pier_version=$(python3 "$pier_root/scripts/release.py" version)
mkdir -p "$pier_output" "$pier_root/target"
pier_output=$(cd -- "$pier_output" && pwd)
pier_bundle=$(mktemp -d "$pier_root/target/upgrade-bundle.XXXXXX")
trap 'rm -rf -- "$pier_bundle"' EXIT
pier_mounts=()
if [ "$pier_service" = controller ]; then
  python3 "$pier_root/scripts/bundle-agent-packages.py" "${5:?six upgraded agent packages are required}" "$pier_bundle"
  pier_mounts=(-v "$pier_bundle:/agent-releases:ro")
fi
source "$pier_root/scripts/package-target.sh"
for pier_system in ubuntu24.04 almalinux8 almalinux9; do
  pier_package_target "$pier_system"
  docker build --platform "linux/$pier_arch" -f "$pier_root/docker/$pier_service-$pier_packager.Dockerfile" \
    -t "pier-$pier_service-package-$pier_packager:$pier_arch" \
    --build-arg http_proxy --build-arg https_proxy --build-arg no_proxy --build-arg NO_PROXY "$pier_root"
  docker run --rm --pull=never --platform "linux/$pier_arch" \
    -v "$pier_root:/src:ro" -v "$pier_root/target/$pier_service-packages-$pier_arch/release:/binary:ro" \
    -v "$pier_output:/out" "${pier_mounts[@]}" \
    "pier-$pier_service-package-$pier_packager:$pier_arch" \
    bash "/src/scripts/package-$pier_service-inner.sh" "$pier_system" "$pier_version" "$pier_revision" "$pier_arch"
done
