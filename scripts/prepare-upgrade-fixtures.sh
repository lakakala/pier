#!/usr/bin/env bash
# Repackage the binary compiled for exactly this system and architecture.
set -euo pipefail
pier_service=${1:?Usage: prepare-upgrade-fixtures.sh agent|controller amd64|arm64 REVISION OUTPUT --system SYSTEM [--agent-packages DIR]}
pier_arch=${2:?architecture}
pier_revision=${3:?revision}
pier_output=${4:?output directory}
shift 4
pier_system=
pier_agent_packages=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --system) pier_system=${2:?system}; shift 2 ;;
    --agent-packages) pier_agent_packages=${2:?six upgraded agent packages are required}; shift 2 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done
case "$pier_service" in agent|controller) ;; *) exit 2 ;; esac
case "$pier_arch" in amd64|arm64) ;; *) exit 2 ;; esac
[[ "$pier_revision" =~ ^[1-9][0-9]*$ ]] || { echo 'revision must be a positive integer' >&2; exit 2; }
pier_root=$(cd -- "$(dirname -- "$0")/.." && pwd)
source "$pier_root/scripts/package-target.sh"
pier_package_target "$pier_system"
pier_binary="$pier_root/target/$pier_service-packages/$pier_system/$pier_arch/release"
test -x "$pier_binary/pier-$pier_service" || { echo 'Build this system and architecture before preparing fixtures.' >&2; exit 2; }
if [ "$pier_service" = controller ] && [ -z "$pier_agent_packages" ]; then
  echo '--agent-packages is required for controller fixtures' >&2; exit 2
fi
pier_version=$(python3 "$pier_root/scripts/release.py" version)
mkdir -p "$pier_output" "$pier_root/target"
pier_output=$(cd -- "$pier_output" && pwd)
pier_bundle=$(mktemp -d "$pier_root/target/upgrade-bundle-$pier_system-$pier_arch.XXXXXX")
trap 'rm -rf -- "$pier_bundle"' EXIT
pier_mounts=()
if [ "$pier_service" = controller ]; then
  python3 "$pier_root/scripts/bundle-agent-packages.py" "$pier_agent_packages" "$pier_bundle"
  pier_mounts=(-v "$pier_bundle:/agent-releases:ro")
fi
docker build --platform "linux/$pier_arch" -f "$pier_root/docker/$pier_service-$pier_packager.Dockerfile" \
  -t "pier-$pier_service-package-$pier_packager:$pier_arch" \
  --build-arg http_proxy --build-arg https_proxy --build-arg no_proxy --build-arg NO_PROXY "$pier_root"
docker run --rm --pull=never --platform "linux/$pier_arch" \
  -v "$pier_root:/src:ro" -v "$pier_binary:/binary:ro" \
  -v "$pier_output:/out" "${pier_mounts[@]}" \
  "pier-$pier_service-package-$pier_packager:$pier_arch" \
  bash "/src/scripts/package-$pier_service-inner.sh" "$pier_system" "$pier_version" "$pier_revision" "$pier_arch"
