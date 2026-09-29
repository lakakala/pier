%global debug_package %{nil}
Name: pier-controller
Version: %{pier_version}
Release: %{pier_release}%{pier_dist}
Summary: Pier deployment controller and web console
License: MIT
Source0: pier-controller
Source1: LICENSE
Source2: pier-controller.service
Source3: README.md
Source4: controller.yml
Source5: api.md
Source6: setup-package.sh
Source7: controller.nginx.conf
Source8: agent.yml
Source9: pier-blueprint.yml
BuildRequires: systemd
Requires: systemd, shadow-utils, git, ca-certificates
%{?systemd_requires}

%description
Manages Git service definitions, builds packages and deploys to Pier agents.
Start pier-controller.service and open /init over HTTPS for first setup.

%install
install -D -m 0755 %{SOURCE0} %{buildroot}%{_bindir}/pier-controller
install -d -m 0755 %{buildroot}%{_datadir}/pier-controller
cp -R %{_sourcedir}/agent-releases %{buildroot}%{_datadir}/pier-controller/
install -D -m 0755 %{SOURCE6} %{buildroot}/usr/lib/pier-controller/setup-package
install -D -m 0644 %{SOURCE4} %{buildroot}/usr/lib/pier-controller/controller.yml
install -D -m 0644 %{SOURCE7} %{buildroot}%{_docdir}/pier-controller/examples/controller.nginx.conf
install -D -m 0644 %{SOURCE2} %{buildroot}%{_unitdir}/pier-controller.service
install -D -m 0644 %{SOURCE1} %{buildroot}%{_licensedir}/pier-controller/LICENSE
install -D -m 0644 %{SOURCE3} %{buildroot}%{_docdir}/pier-controller/README.md
install -D -m 0644 %{SOURCE8} %{buildroot}%{_docdir}/pier-controller/examples/agent.yml
install -D -m 0644 %{SOURCE9} %{buildroot}%{_docdir}/pier-controller/examples/pier-blueprint.yml
install -D -m 0644 %{SOURCE4} %{buildroot}%{_docdir}/pier-controller/examples/controller.yml
install -D -m 0644 %{SOURCE5} %{buildroot}%{_docdir}/pier-controller/api.md
install -d -m 0755 %{buildroot}%{_sysconfdir}/pier

%post
# No preset, enable, start or restart.
/usr/lib/pier-controller/setup-package || exit $?
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || :
fi

%preun
%systemd_preun pier-controller.service

%postun
%systemd_postun pier-controller.service

%files
%{_bindir}/pier-controller
%{_datadir}/pier-controller/
/usr/lib/pier-controller/
%{_unitdir}/pier-controller.service
%dir %{_sysconfdir}/pier
%license %{_licensedir}/pier-controller/LICENSE
%doc %{_docdir}/pier-controller/
