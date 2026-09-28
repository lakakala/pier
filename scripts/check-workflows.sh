#!/usr/bin/env bash
set -euo pipefail
pier_root=$(cd -- "$(dirname -- "$0")/.." && pwd)
case "$(uname -s)/$(uname -m)" in
  Linux/x86_64)
    pier_arch=amd64
    pier_digest=8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8 ;;
  Linux/aarch64)
    pier_arch=arm64
    pier_digest=325e971b6ba9bfa504672e29be93c24981eeb1c07576d730e9f7c8805afff0c6 ;;
  *) echo 'Workflow checks require Linux amd64 or arm64.' >&2; exit 2 ;;
esac
pier_temp=$(mktemp -d)
trap 'rm -rf -- "$pier_temp"' EXIT
curl --fail --silent --show-error --location --retry 3 \
  "https://github.com/rhysd/actionlint/releases/download/v1.7.12/actionlint_1.7.12_linux_$pier_arch.tar.gz" \
  -o "$pier_temp/actionlint.tar.gz"
printf '%s  %s\n' "$pier_digest" "$pier_temp/actionlint.tar.gz" | sha256sum --check
tar -xzf "$pier_temp/actionlint.tar.gz" -C "$pier_temp" actionlint
cd "$pier_root"
# GitHub supports concurrency.queue, but actionlint 1.7.12 does not yet know it.
# Ignore only that diagnostic; all other workflow and shell checks still apply.
# https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency
"$pier_temp/actionlint" \
  -ignore '^unexpected key "queue" for "concurrency" section\. expected one of "cancel-in-progress", "group"$' \
  .github/workflows/*.yml
