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
cp /binary/pier-agent /build/prebuilt/pier-agent
cp /src/LICENSE /build/prebuilt/LICENSE
cp /src/docs/services.md /build/prebuilt/README.md
cp /src/docs/api.md /build/prebuilt/api.md
cp /src/examples/services/agent.yml /build/prebuilt/agent.yml
cp /src/packaging/agent/pier-agent-upgrade.service /build/prebuilt/pier-agent-upgrade.service
# Fail instead of publishing a package built against an incompatible libc.
python3 /src/scripts/check-agent-elf.py /binary/pier-agent "$pier_arch" "$pier_system"
case "$pier_package" in
  deb)
    cp -R /src/packaging/agent/debian /build/debian
    cp /src/packaging/agent/pier-agent.service /build/debian/pier-agent.service
    cp /src/packaging/agent/needrestart.conf /build/prebuilt/needrestart.conf
    chmod +x /build/debian/rules /build/debian/postinst /build/debian/prerm
    python3 - "$pier_version" "$pier_revision" <<'PY'
import email.utils, os, sys
version, revision = sys.argv[1:]
date = email.utils.formatdate(int(os.environ['SOURCE_DATE_EPOCH']), usegmt=False)
with open('/build/debian/changelog', 'w') as out:
    out.write('pier-agent (%s-%s.ubuntu24.04) noble; urgency=medium\n\n  * Build Pier deployment agent.\n\n -- Pier maintainers <pier@localhost>  %s\n' % (version, revision, date))
PY
    dpkg-buildpackage -b --no-sign
    cp /pier-agent_*.deb /out/
    ;;
  rpm)
    mkdir -p /build/rpmbuild/SOURCES
    cp /build/prebuilt/* /build/rpmbuild/SOURCES/
    cp /src/packaging/agent/pier-agent.service /build/rpmbuild/SOURCES/
    rpmbuild -bb /src/packaging/agent/rpm/pier-agent.spec \
      --define '_topdir /build/rpmbuild' --define "pier_version $pier_version" --define "pier_release $pier_revision" --define "pier_dist $pier_dist"
    cp /build/rpmbuild/RPMS/*/pier-agent-*.rpm /out/
    ;;
  *) exit 2 ;;
esac
