#!/usr/bin/env bash
# Internal target mapping shared by the Actions packaging steps.
pier_package_target() {
  [ -n "$1" ] || { echo '--system is required' >&2; return 2; }
  case "$1" in
    ubuntu24.04)
      pier_package=deb; pier_packager=deb; pier_dist=.ubuntu24.04
      pier_distro=ubuntu2404; pier_builder_image=pier-builder-rust:ubuntu24.04 ;;
    almalinux8)
      pier_package=rpm; pier_packager=rpm; pier_dist=.el8
      pier_distro=almalinux8; pier_builder_image=pier-builder-rust:almalinux8 ;;
    almalinux9)
      pier_package=rpm; pier_packager=rpm-almalinux9; pier_dist=.el9
      pier_distro=almalinux9; pier_builder_image=pier-builder-rust:almalinux9 ;;
    *) echo "Unsupported package system: $1" >&2; return 2 ;;
  esac
  pier_builder_dockerfile="docker/rust-$pier_distro.Dockerfile"
}
