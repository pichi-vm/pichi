# Copr SCM: method "make srpm" (.copr/Makefile), spec packaging/pichi.spec.
# Vendor is generated at SRPM time (not committed): see .copr/Makefile.
#
# Scaffolded from rust2rpm 28, hand-tuned for this workspace app:
# - package name pichi; CLI only (no library install)
# - Source0 is the git tree; Source1 is the SRPM-time vendor/ tarball
# - %cargo_prep -v vendor; git/crates.io replacements come from the vendor step
# - rpm-manifest.patch and lock pruning drop vm-tests deps for the host CLI
# - EPEL / CentOS Stream: rust-toolset; Fedora: cargo-rpm-macros

%bcond_with check

%global cargo_install_lib 0
%global crate pichi

Name:           pichi
Version:        0.1.0
Release:        1%{?dist}
Summary:        Front-end for pichi VM images

License:        Apache-2.0 AND MIT AND Unicode-3.0 AND Zlib AND ISC AND BSD-3-Clause AND Unlicense AND BSL-1.0 AND CDLA-Permissive-2.0
URL:            https://github.com/pichi-vm/pichi
Source0:        %{crate}-%{version}.tar.gz
Source1:        %{crate}-%{version}-vendor.tar.xz

%if 0%{?rhel}
BuildRequires:  rust-toolset
%else
BuildRequires:  cargo-rpm-macros >= 26
%endif
BuildRequires:  cmake
BuildRequires:  gcc
BuildRequires:  gcc-c++
BuildRequires:  make
# Installing pichi also pulls in the dillo-vm VMM package.
Requires:       dillo-vm

ExclusiveArch:  x86_64 aarch64

%global _description %{expand:
Host CLI that manages pichi VM images as OCI artifacts and launches them.
This package does not ship guest images.}

%description %{_description}

%prep
# Source0 is already patch-applied in .copr/Makefile before cargo vendor.
%autosetup -n %{crate}-%{version} -p1 -a1
%cargo_prep -v vendor
# cargo vendor (SRPM time) emits source replacements; append if present.
if [ -f packaging/cargo-vendor-config.toml ]; then
  printf '\n' >> .cargo/config.toml
  cat packaging/cargo-vendor-config.toml >> .cargo/config.toml
fi
# Fedora cargo-rpm-macros already set [net] offline; EL
# may not. Skip a second [net] table if offline is present.
grep -q '^offline' .cargo/config.toml || printf '\n[net]\noffline = true\n' >> .cargo/config.toml

%build
export CARGO_NET_OFFLINE=true
%cargo_build -- --locked

%install
export CARGO_NET_OFFLINE=true
%cargo_install -- --locked

%if %{with check}
%check
export CARGO_NET_OFFLINE=true
%cargo_test -- --locked
%endif

%files
%license LICENSE
%doc README.md
%{_bindir}/pichi

%changelog
* Tue Oct 27 2026 Nathaniel McCallum <nathaniel@mccallum.life> - 0.1.0-1
- Add Copr packaging; vendor generated at SRPM time
