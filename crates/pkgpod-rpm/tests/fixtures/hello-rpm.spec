# SPDX-License-Identifier: AGPL-3.0-only
# A test package; make-fixtures.sh builds it.
%global debug_package %{nil}
Name:           %{pkg}
Version:        %{ver}
Release:        1
Summary:        Test package %{pkg}
License:        MIT
URL:            https://example.org/
%if "%{pkg}" == "tool-rpm"
Requires:       hello-rpm >= 1.0
%else
BuildArch:      noarch
Provides:       greeting = %{ver}
%endif

%description
A package for pkgpod's tests.

%install
mkdir -p %{buildroot}/usr/bin %{buildroot}/etc
printf '#!/bin/sh\necho "%{pkg} %{ver}"\n' > %{buildroot}/usr/bin/%{pkg}
chmod 755 %{buildroot}/usr/bin/%{pkg}
echo "%{pkg}" > %{buildroot}/etc/%{pkg}.conf

%files
/usr/bin/%{pkg}
%config(noreplace) /etc/%{pkg}.conf

%changelog
* Mon Nov 23 2026 pkgpod tests <tests@example.org> - %{ver}-1
- A test build.
