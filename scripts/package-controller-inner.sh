#!/usr/bin/env bash
set -euo pipefail
pier_system=${1:?system}
source /src/scripts/package-target.sh
pier_package_target "$pier_system"
pier_version=${2:?version}
pier_revision=${3:?revision}
pier_arch=${4:?architecture}
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-0}
mkdir -p /build/prebuilt
test -f /agent-releases/manifest.json
cp -R /agent-releases /build/prebuilt/agent-releases
cp /binary/pier-controller /build/prebuilt/pier-controller
cp /src/LICENSE /build/prebuilt/LICENSE
sed -e 's|../examples/services/|examples/|g' -e 's|../examples/blueprints/web/|examples/|g' /src/docs/services.md > /build/prebuilt/README.md
cp /src/docs/api.md /build/prebuilt/api.md
cp /src/examples/services/controller.yml /build/prebuilt/controller.yml
cp /src/examples/services/agent.yml /build/prebuilt/agent.yml
cp /src/examples/blueprints/web/pier-blueprint.yml /build/prebuilt/pier-blueprint.yml
cp /src/examples/services/controller.nginx.conf /build/prebuilt/controller.nginx.conf
cp /src/packaging/controller/setup-package.sh /build/prebuilt/setup-package.sh
# Fail instead of publishing a package built against an incompatible libc.
python3 /src/scripts/check-agent-elf.py /binary/pier-controller "$pier_arch" "$pier_system"
case "$pier_package" in
  deb)
    cp -R /src/packaging/controller/debian /build/debian
    cp /src/packaging/controller/pier-controller.service /build/debian/pier-controller.service
    cp /src/packaging/controller/needrestart.conf /build/prebuilt/needrestart.conf
    chmod +x /build/debian/rules /build/debian/postinst /build/debian/prerm
    python3 - "$pier_version" "$pier_revision" <<'PY'
import email.utils, os, sys
version, revision = sys.argv[1:]
date = email.utils.formatdate(int(os.environ['SOURCE_DATE_EPOCH']), usegmt=False)
with open('/build/debian/changelog', 'w') as out:
    out.write('pier-controller (%s-%s.ubuntu24.04) noble; urgency=medium\n\n  * Build Pier deployment controller.\n\n -- Pier maintainers <pier@localhost>  %s\n' % (version, revision, date))
PY
    dpkg-buildpackage -b --no-sign
    cp /pier-controller_*.deb /out/
    ;;
  rpm)
    mkdir -p /build/rpmbuild/SOURCES
    cp -R /build/prebuilt/* /build/rpmbuild/SOURCES/
    cp /src/packaging/controller/pier-controller.service /build/rpmbuild/SOURCES/
    rpmbuild -bb /src/packaging/controller/rpm/pier-controller.spec \
      --define '_topdir /build/rpmbuild' --define "pier_version $pier_version" --define "pier_release $pier_revision" --define "pier_dist $pier_dist"
    cp /build/rpmbuild/RPMS/*/pier-controller-*.rpm /out/
    ;;
  *) exit 2 ;;
esac
