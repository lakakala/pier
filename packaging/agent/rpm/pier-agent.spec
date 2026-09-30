%global debug_package %{nil}
Name: pier-agent
Version: %{pier_version}
Release: %{pier_release}%{pier_dist}
Summary: Pier deployment agent and application supervisor
License: MIT
Source0: pier-agent
Source1: LICENSE
Source2: pier-agent.service
Source3: README.md
Source4: agent.yml
Source5: api.md
Source6: pier-agent-upgrade.service
BuildRequires: systemd
Requires: systemd, shadow-utils, glibc-common, bash
%{?systemd_requires}

%description
Receives encrypted deployments from pier-controller and supervises applications
under individual system accounts. Run sudo pier-agent init after installation.

%install
install -D -m 0755 %{SOURCE0} %{buildroot}%{_bindir}/pier-agent
install -D -m 0644 %{SOURCE2} %{buildroot}%{_unitdir}/pier-agent.service
install -D -m 0644 %{SOURCE6} %{buildroot}%{_unitdir}/pier-agent-upgrade.service
install -D -m 0644 %{SOURCE1} %{buildroot}%{_licensedir}/pier-agent/LICENSE
install -D -m 0644 %{SOURCE3} %{buildroot}%{_docdir}/pier-agent/README.md
install -D -m 0644 %{SOURCE4} %{buildroot}%{_docdir}/pier-agent/examples/agent.yml
install -D -m 0644 %{SOURCE5} %{buildroot}%{_docdir}/pier-agent/api.md
install -d -m 0755 %{buildroot}%{_sysconfdir}/pier

%post
# No preset, enable or start: enrollment is explicit and interactive.
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || :
fi
echo 'pier-agent installed. First setup: sudo pier-agent init'
echo 'Package scripts leave the running agent unchanged. Automatic upgrades restart it through pier-agent-upgrade.service.'
echo 'After a manual installation: sudo systemctl restart pier-agent'

%preun
%systemd_preun pier-agent-upgrade.service pier-agent.service

%postun
%systemd_postun pier-agent-upgrade.service pier-agent.service

%files
%{_bindir}/pier-agent
%{_unitdir}/pier-agent.service
%{_unitdir}/pier-agent-upgrade.service
%dir %{_sysconfdir}/pier
%license %{_licensedir}/pier-agent/LICENSE
%doc %{_docdir}/pier-agent/
