#!/usr/bin/env bash
# Internal target mapping shared by the Actions packaging steps.
pier_package_target() {
  case "$1" in
    ubuntu24.04) pier_package=deb; pier_packager=deb; pier_dist=.ubuntu24.04 ;;
    almalinux8) pier_package=rpm; pier_packager=rpm; pier_dist=.el8 ;;
    almalinux9) pier_package=rpm; pier_packager=rpm-almalinux9; pier_dist=.el9 ;;
    *) echo "Unsupported package system: $1" >&2; return 2 ;;
  esac
}
