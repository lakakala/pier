#!/usr/bin/env bash
set -euo pipefail
pier_image=
pier_arch=
pier_system=
pier_revision=1
pier_output=
pier_agent_packages=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --image) pier_image=${2:?image}; shift 2 ;;
    --arch) pier_arch=${2:?architecture}; shift 2 ;;
    --system) pier_system=${2:?system}; shift 2 ;;
    --revision) pier_revision=${2:?revision}; shift 2 ;;
    --output) pier_output=${2:?directory}; shift 2 ;;
    --agent-packages) pier_agent_packages=${2:?directory}; shift 2 ;;
    -h|--help) echo 'Usage: bash scripts/package-controller.sh --image IMAGE --arch amd64|arm64 --system ubuntu24.04|almalinux8|almalinux9 [--revision N] [--output DIR] [--agent-packages DIR (default: dist/)]'; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ -n "$pier_image" ] || { echo '--image is required' >&2; exit 2; }
case "$pier_arch" in amd64|arm64) ;; *) echo '--arch must be amd64 or arm64' >&2; exit 2 ;; esac
[[ "$pier_revision" =~ ^[1-9][0-9]*$ ]] || { echo 'revision must be a positive integer' >&2; exit 2; }
pier_repository=$(cd -- "$(dirname -- "$0")/.." && pwd)
source "$pier_repository/scripts/package-target.sh"
pier_package_target "$pier_system"
pier_version=$(python3 "$pier_repository/scripts/release.py" version)
pier_output=${pier_output:-$pier_repository/dist}
mkdir -p "$pier_output"
pier_output=$(cd -- "$pier_output" && pwd)
pier_agent_packages=${pier_agent_packages:-$pier_repository/dist}
mkdir -p "$pier_repository/target"
pier_bundle=$(mktemp -d "$pier_repository/target/controller-bundle-$pier_system-$pier_arch.XXXXXX")
trap 'rm -rf -- "$pier_bundle"' EXIT
python3 "$pier_repository/scripts/bundle-agent-packages.py" "$pier_agent_packages" "$pier_bundle"
# Cargo does not fingerprint the container's libc/linker. An image change must
# select a different cache even when both images contain the same Rust version.
pier_image_ref=$(docker image inspect --format '{{.Id}}' "$pier_image")
pier_image_id=$(docker image inspect --platform "linux/$pier_arch" --format '{{.Id}}' "$pier_image_ref")
[[ "$pier_image_id" =~ ^sha256:[0-9a-f]{64}$ ]] || { echo 'Cannot determine build image identity' >&2; exit 2; }
pier_target="$pier_repository/target/controller-builds/$pier_system/$pier_arch/${pier_image_id#sha256:}"
pier_cache="$pier_repository/target/controller-packages-cache/$pier_system/$pier_arch/${pier_image_id#sha256:}"
pier_registry=${CARGO_HOME:-${HOME}/.cargo}/registry
mkdir -p "$pier_target" "$pier_cache"
# Explicit build image and architecture; offline dependencies are supplied by cargo fetch --locked.
docker run --rm --pull=never --platform "linux/$pier_arch" --user "$(id -u):$(id -g)" \
  -e CARGO_HOME=/cache -e CARGO_TARGET_DIR=/out -e CARGO_BUILD_JOBS=4 \
  -v "$pier_repository:/src:ro" -v "$pier_target:/out" -v "$pier_cache:/cache" -v "$pier_registry:/cache/registry:ro" \
  -w /src "$pier_image_ref" cargo build -p pier-controller --release --offline --locked
docker build --platform "linux/$pier_arch" -f "$pier_repository/docker/controller-$pier_packager.Dockerfile" \
  -t "pier-controller-package-$pier_packager:$pier_arch" --build-arg http_proxy --build-arg https_proxy --build-arg no_proxy --build-arg NO_PROXY "$pier_repository"
docker run --rm --pull=never --platform "linux/$pier_arch" \
  -v "$pier_repository:/src:ro" -v "$pier_target/release:/binary:ro" -v "$pier_output:/out" \
  -v "$pier_bundle:/agent-releases:ro" \
  "pier-controller-package-$pier_packager:$pier_arch" bash /src/scripts/package-controller-inner.sh "$pier_system" "$pier_version" "$pier_revision" "$pier_arch"
# Stable location for the package lifecycle test fixtures, separate from caches.
mkdir -p "$pier_repository/target/controller-packages/$pier_system/$pier_arch/release"
install -m 0755 "$pier_target/release/pier-controller" "$pier_repository/target/controller-packages/$pier_system/$pier_arch/release/pier-controller"
printf 'Packages written to %s\n' "$pier_output"
python3 - "$pier_output" <<'PY'
import hashlib, os, pathlib, sys, tempfile
root = pathlib.Path(sys.argv[1])
packages = sorted(list(root.glob('*.deb')) + list(root.glob('*.rpm')))
with tempfile.NamedTemporaryFile('w', dir=root, delete=False) as out:
    for package in packages:
        out.write('%s  %s\n' % (hashlib.sha256(package.read_bytes()).hexdigest(), package.name))
    temporary = out.name
os.replace(temporary, root / 'SHA256SUMS')
os.chmod(root / 'SHA256SUMS', 0o644)
PY
